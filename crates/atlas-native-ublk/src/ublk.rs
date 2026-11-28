// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The kernel side, over `libublk`: one thread per hardware queue fetches requests, hands them to
//! the [`Dispatcher`] and commits each result when the worker signals the request's eventfd
//! through the queue's io_uring. Linux 6.0 or newer with `ublk_drv`, and `CAP_SYS_ADMIN`.

use std::{
    fs::OpenOptions,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    thread,
};

use io_uring::{opcode, squeue, types, IoUring};
use libublk::{
    ctrl::{UblkCtrl, UblkCtrlBuilder},
    helpers::IoBuf,
    io::{UblkDev, UblkQueue},
    sys, BufDesc, UblkError, UblkFlags,
};

use crate::{
    dispatch::Dispatcher,
    params::{completion, device_params, DeviceParams, SECTOR},
    volume::{Op, Reply, Request, Volume},
};

/// `include/uapi/linux/prctl.h`.
const PR_SET_IO_FLUSHER: libc::c_int = 57;

#[derive(Debug, Clone, Copy)]
pub struct DeviceOptions {
    /// Device number to ask for, or -1 for the first free one.
    pub id: i32,
    pub queues: u16,
    pub depth: u16,
    /// Largest read or write the device accepts.
    pub io_bytes: u32,
}

/// What a signal acts on, filled in as the process gets there.
#[derive(Default)]
struct Running {
    ctrl: Option<Arc<UblkCtrl>>,
    volume: Option<Arc<Volume>>,
    signals: u32,
}

/// Process-wide setup that has to happen while there is only one thread.
pub struct Process {
    running: Arc<Mutex<Running>>,
}

fn lock(m: &Mutex<Running>) -> MutexGuard<'_, Running> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stops the device, from any thread. libublk sends control commands through a ring that is
/// per thread and only set up for the thread that created the device.
fn stop(ctrl: &UblkCtrl) -> Result<(), UblkError> {
    libublk::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            *ring = Some(
                IoUring::<squeue::Entry128>::builder()
                    .build(16)
                    .map_err(UblkError::IOError)?,
            );
        }
        Ok(())
    })?;
    ctrl.kill_dev().map(drop)
}

/// Removes device `id` after its server died without removing it, which the kernel keeps until
/// asked. Refuses a device that is not stopped, or that a process still serves: removing it would
/// take it away from that server (also one that is just creating it).
pub fn delete_device(id: u32) -> Result<(), String> {
    let number = i32::try_from(id).map_err(|_| format!("device number {id} is out of range"))?;
    let ctrl =
        UblkCtrl::new_simple(number).map_err(|e| format!("no ublk device {id} to remove: {e}"))?;
    let state = u32::from(ctrl.dev_info().state);
    if state != sys::UBLK_S_DEV_DEAD {
        return Err(format!(
            "ublk device {id} is not stopped (state {state}); stop its server instead"
        ));
    }
    // The control node admits one opener, the server. Closed again before the removal, which
    // waits for every holder.
    let cdev = ctrl.get_cdev_path();
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&cdev)
        .map_err(|e| format!("{cdev}: {e}; a process still serves the device"))?;
    ctrl.del_dev()
        .map(drop)
        .map_err(|e| format!("removing ublk device {id}: {e}"))
}

/// A server that shares a mount namespace with users of its own device can deadlock when it
/// exits (the exit unmounts, and the unmount waits for I/O only this process can serve).
///
/// Mounts are made slaves rather than private: nothing mounted here reaches the caller, but an
/// unmount there reaches here. With private mounts this process would keep every filesystem that
/// existed when it started mounted after its owner unmounted it, another ublk device's included,
/// whose data would then not be written back by that unmount.
fn own_mount_namespace() -> io::Result<()> {
    // SAFETY: plain system calls with constant, NUL-terminated or null arguments.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_SLAVE,
            std::ptr::null(),
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Process {
    /// Call first thing in `main`: every thread started later inherits what this sets.
    ///
    /// SIGINT, SIGTERM and SIGHUP are taken over by one thread. Before the device exists they
    /// end the process. Afterwards the first stops the device, still serving what the kernel has
    /// in flight or writes back while stopping; a second also gives up on the cluster, failing
    /// those requests.
    pub fn prepare(share_mount_namespace: bool) -> Result<Self, String> {
        if !share_mount_namespace {
            own_mount_namespace()
                .map_err(|e| format!("own mount namespace (needs CAP_SYS_ADMIN): {e}"))?;
        }
        // Memory this process allocates while serving writeback must not wait on that writeback.
        // SAFETY: prctl with integer arguments.
        if unsafe { libc::prctl(PR_SET_IO_FLUSHER, 1, 0, 0, 0) } != 0 {
            tracing::warn!(
                "PR_SET_IO_FLUSHER failed ({}); under memory pressure writes can deadlock",
                io::Error::last_os_error()
            );
        }
        // SAFETY: the set is initialised by sigemptyset before use.
        let set = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::sigaddset(&mut set, s);
            }
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if rc != 0 {
                return Err(format!(
                    "blocking signals: {}",
                    io::Error::from_raw_os_error(rc)
                ));
            }
            set
        };
        let running = Arc::new(Mutex::new(Running::default()));
        let shared = running.clone();
        thread::Builder::new()
            .name("ublk-signals".into())
            .spawn(move || loop {
                let mut signal = 0;
                // SAFETY: both pointers are to live locals.
                if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
                    continue;
                }
                let mut r = lock(&shared);
                r.signals += 1;
                let Some(ctrl) = r.ctrl.clone() else {
                    std::process::exit(128 + signal);
                };
                if r.signals > 1 {
                    tracing::warn!("second signal: failing requests still waiting for the cluster");
                    if let Some(v) = &r.volume {
                        v.abort();
                    }
                } else {
                    tracing::info!("stopping the device; signal again to fail waiting requests");
                }
                drop(r);
                // Stopping waits for requests in flight, so it must not hold up this thread.
                thread::spawn(move || {
                    if let Err(e) = stop(&ctrl) {
                        tracing::warn!("stopping the device: {e}");
                    }
                });
            })
            .map_err(|e| format!("signal thread: {e}"))?;
        Ok(Self { running })
    }

    /// Creates `/dev/ublkbN` for `volume` and serves it until the device is stopped, then
    /// removes it. `ready` is called with the device's path once it exists.
    pub fn serve(
        &self,
        volume: Arc<Volume>,
        opts: DeviceOptions,
        ready: impl FnOnce(&str) + Send + Sync + 'static,
    ) -> Result<(), String> {
        let workers = usize::from(opts.queues) * usize::from(opts.depth);
        let pool = Arc::new(
            Dispatcher::new(workers, {
                let volume = volume.clone();
                Arc::new(move |r| volume.execute(r))
            })
            .map_err(|e| format!("I/O threads: {e}"))?,
        );
        let ctrl = {
            // Held across creation so a signal cannot end the process between creating the
            // device and recording it.
            let mut running = lock(&self.running);
            let ctrl = UblkCtrlBuilder::default()
                .name("atlas-native")
                .id(opts.id)
                .nr_queues(opts.queues)
                .depth(opts.depth)
                .io_buf_bytes(opts.io_bytes)
                .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
                .build()
                .map_err(|e| format!("creating the ublk device (is ublk_drv loaded?): {e}"))?;
            let ctrl = Arc::new(ctrl);
            running.ctrl = Some(ctrl.clone());
            running.volume = Some(volume.clone());
            ctrl
        };
        let geometry = volume.geometry();
        let read_only = volume.read_only();
        let path = ctrl.get_bdev_path();
        let running = self.running.clone();
        let served = ctrl.run_target(
            move |dev: &mut UblkDev| {
                let buffer = dev.dev_info.max_io_buf_bytes;
                apply(dev, device_params(geometry, buffer, read_only));
                Ok(())
            },
            move |qid, dev: &UblkDev| queue(qid, dev, &pool),
            move |ctrl: &UblkCtrl| {
                ready(&path);
                // A signal that came while the device was starting had nothing to stop yet.
                if lock(&running).signals > 0 {
                    if let Err(e) = ctrl.kill_dev() {
                        tracing::warn!("stopping the device: {e}");
                    }
                }
            },
        );
        if let Err(e) = served {
            // The queue threads may still be up, so removal must not wait for them.
            let _ = ctrl.kill_dev();
            let _ = ctrl.del_dev_async();
            return Err(format!("serving the device: {e}"));
        }
        ctrl.del_dev()
            .map_err(|e| format!("removing the stopped device: {e}"))?;
        lock(&self.running).ctrl = None;
        Ok(())
    }
}

fn apply(dev: &mut UblkDev, p: DeviceParams) {
    let mut attrs = 0;
    if p.read_only {
        attrs |= sys::UBLK_ATTR_READ_ONLY;
    }
    let mut params = sys::ublk_params {
        types: sys::UBLK_PARAM_TYPE_BASIC,
        basic: sys::ublk_param_basic {
            attrs,
            logical_bs_shift: p.logical_bs_shift,
            physical_bs_shift: p.physical_bs_shift,
            io_opt_shift: p.io_opt_shift,
            io_min_shift: p.io_min_shift,
            max_sectors: p.max_sectors,
            dev_sectors: p.dev_sectors,
            ..Default::default()
        },
        ..Default::default()
    };
    if p.max_write_zeroes_sectors > 0 {
        // Write-zeroes only: with no discard sectors the device refuses discard.
        params.types |= sys::UBLK_PARAM_TYPE_DISCARD;
        params.discard = sys::ublk_param_discard {
            discard_granularity: 1 << p.logical_bs_shift,
            max_write_zeroes_sectors: p.max_write_zeroes_sectors,
            ..Default::default()
        };
    }
    dev.tgt.dev_size = p.dev_sectors * SECTOR;
    dev.tgt.params = params;
}

/// An eventfd that a worker signals and the queue's ring polls. Shared, so the descriptor stays
/// open for as long as a worker may still write to it.
struct Event(OwnedFd);

impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd has no memory arguments; a non-negative result is a new descriptor.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is open and owned by nothing else.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn signal(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes the 8 bytes of `one`.
        let n = unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
        if n != 8 {
            tracing::error!("signalling a completion: {}", io::Error::last_os_error());
        }
    }

    fn clear(&self) {
        let mut count = [0u8; 8];
        // SAFETY: reads at most the 8 bytes of `count`; the descriptor is non-blocking.
        unsafe { libc::read(self.0.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
    }
}

fn queue(qid: u16, dev: &UblkDev, pool: &Arc<Dispatcher>) {
    let q = match UblkQueue::new(qid, dev) {
        Ok(q) => Rc::new(q),
        Err(e) => {
            // The device cannot start without every queue, and starting would wait for this one.
            tracing::error!(
                "setting up queue {qid} of ublk device {}: {e}",
                dev.dev_info.dev_id
            );
            std::process::exit(1);
        }
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let tasks: Vec<_> = q
        .tags()
        .map(|tag| {
            let (q, pool) = (q.clone(), pool.clone());
            executor.spawn(async move {
                match serve_tag(&q, tag, &pool).await {
                    Ok(()) | Err(UblkError::QueueIsDown) => {}
                    Err(e) => tracing::error!("queue {} tag {tag}: {e}", q.get_qid()),
                }
            })
        })
        .collect();
    let ticking = executor.clone();
    smol::block_on(executor.run(async move {
        let run_ops = || while ticking.try_tick() {};
        let done = || tasks.iter().all(|t| t.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&q, Some(20), run_ops, done).await {
            tracing::error!("queue {qid}: {e}");
        }
    }));
}

/// One tag's life: fetch a request, run it, commit its result and fetch the next.
async fn serve_tag(q: &UblkQueue<'_>, tag: u16, pool: &Dispatcher) -> Result<(), UblkError> {
    let event = Arc::new(Event::new().map_err(UblkError::IOError)?);
    let mut buf = IoBuf::<u8>::new(q.dev.dev_info.max_io_buf_bytes as usize);
    q.submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(&buf))
        .await?;
    loop {
        // Copied out at once: the descriptor is shared memory, and the buffer is not relied on
        // across an await (an idle queue may drop its pages).
        let iod = q.get_iod(tag);
        let (code, sector, sectors) = (iod.op_flags & 0xff, iod.start_sector, iod.nr_sectors);
        let op = match code {
            sys::UBLK_IO_OP_READ => Some(Op::Read),
            sys::UBLK_IO_OP_WRITE => Some(Op::Write),
            sys::UBLK_IO_OP_FLUSH => Some(Op::Flush),
            sys::UBLK_IO_OP_DISCARD => Some(Op::Discard),
            sys::UBLK_IO_OP_WRITE_ZEROES => Some(Op::WriteZeroes),
            _ => None,
        };
        let len = u64::from(sectors) * SECTOR;
        let transfers = matches!(op, Some(Op::Read | Op::Write));
        let result = match op {
            None => -libc::EOPNOTSUPP,
            Some(_) if transfers && len > buf.as_slice().len() as u64 => -libc::EINVAL,
            Some(op) => {
                let data = match op {
                    Op::Write => buf.as_slice()[..len as usize].to_vec(),
                    _ => Vec::new(),
                };
                let request = Request {
                    op,
                    offset: sector * SECTOR,
                    len,
                    data,
                };
                let reply = run(q, pool, &event, request).await?;
                if let (Op::Read, Ok(data)) = (op, &reply) {
                    if data.len() as u64 == len {
                        buf.as_mut_slice()[..data.len()].copy_from_slice(data);
                    }
                }
                completion(op, len, &reply)
            }
        };
        q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), result)
            .await?;
    }
}

async fn run(
    q: &UblkQueue<'_>,
    pool: &Dispatcher,
    event: &Arc<Event>,
    request: Request,
) -> Result<Reply, UblkError> {
    let slot = Arc::new(Mutex::new(None));
    pool.submit(request, {
        let (slot, event) = (slot.clone(), event.clone());
        move |reply| {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
            event.signal();
        }
    });
    loop {
        if let Some(reply) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Ok(reply);
        }
        let poll = opcode::PollAdd::new(types::Fd(event.0.as_raw_fd()), libc::POLLIN as u32);
        let ready = q.ublk_submit_sqe(poll.build()).await;
        if ready < 0 && ready != -libc::EINTR && ready != -libc::EAGAIN {
            return Err(UblkError::OtherError(ready));
        }
        // A signal left over from an earlier request only costs one more turn of this loop.
        event.clear();
    }
}
