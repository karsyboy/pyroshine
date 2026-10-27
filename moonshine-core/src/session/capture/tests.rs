//! Ownership and failure semantics every capture backend inherits from
//! [`super::start_capture_thread`], exercised with mock backends: no GPU,
//! compositor or XWayland is needed.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use super::admission::capture_channel;
use super::frame::{ExportedFrame, FrameColorSpace};
use super::start_capture_thread;
use crate::session::manager::SessionShutdownReason;

const REASON: SessionShutdownReason = SessionShutdownReason::CompositorStopped;
const READY: Duration = Duration::from_secs(5);

/// Counts drops of a resource the mock backend owns.
struct Owned(Arc<AtomicUsize>);
impl Drop for Owned {
	fn drop(&mut self) {
		self.0.fetch_add(1, Ordering::SeqCst);
	}
}

thread_local! {
	static THREAD_STATE: std::cell::RefCell<Option<ThreadState>> = const { std::cell::RefCell::new(None) };
}

/// Stands in for driver state released by thread-local destructors after the
/// thread body returns (EGL on NVIDIA). Records when its release finished.
struct ThreadState(Arc<Mutex<Option<Instant>>>);
impl Drop for ThreadState {
	fn drop(&mut self) {
		std::thread::sleep(Duration::from_millis(50));
		*self.0.lock().unwrap() = Some(Instant::now());
	}
}

fn wait_until_stopped(stop: &ShutdownManager<SessionShutdownReason>) {
	while !stop.is_shutdown_triggered() {
		std::thread::sleep(Duration::from_millis(1));
	}
}

fn completed(stop: &ShutdownManager<SessionShutdownReason>) -> Instant {
	tokio::runtime::Builder::new_current_thread()
		.enable_time()
		.build()
		.unwrap()
		.block_on(async {
			tokio::time::timeout(Duration::from_secs(5), stop.wait_shutdown_complete())
				.await
				.expect("session completion")
		});
	Instant::now()
}

/// A single-plane DMA-BUF backed by an anonymous file.
fn dmabuf() -> Dmabuf {
	let file: OwnedFd = tempfile::tempfile().unwrap().into();
	let mut builder = Dmabuf::builder((64, 64), Fourcc::Xrgb8888, Modifier::Linear, DmabufFlags::empty());
	assert!(builder.add_plane(file, 0, 256));
	builder.build().unwrap()
}

fn identity(fd: BorrowedFd<'_>) -> (u64, u64) {
	let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
	// SAFETY: fd is open for the borrow and `stat` is a valid out pointer.
	assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) }, 0);
	// SAFETY: fstat succeeded and initialized the structure.
	let stat = unsafe { stat.assume_init() };
	(stat.st_dev, stat.st_ino)
}

#[test]
fn ready_backend_runs_until_stop_and_completion_follows_its_thread_exit() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let drops = Arc::new(AtomicUsize::new(0));
	let thread_released = Arc::new(Mutex::new(None));
	let ready = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		let (stop, drops, thread_released) = (stop.clone(), drops.clone(), thread_released.clone());
		move |ready: mpsc::SyncSender<&'static str>| {
			let _owned = Owned(drops);
			THREAD_STATE.with(|state| *state.borrow_mut() = Some(ThreadState(thread_released)));
			ready.send("ready").unwrap();
			wait_until_stopped(&stop);
			Ok(())
		}
	});
	assert_eq!(ready, Ok("ready"));
	assert!(!stop.is_shutdown_triggered(), "readiness does not stop the session");
	assert_eq!(tx.context_owners(), 3, "both channel ends and the backend's hold");

	stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	let completed_at = completed(&stop);
	assert_eq!(drops.load(Ordering::SeqCst), 1, "backend resources released once");
	let released_at = thread_released
		.lock()
		.unwrap()
		.expect("thread-local state released before completion");
	assert!(released_at <= completed_at);
	assert_eq!(
		tx.context_owners(),
		2,
		"the capture GPU hold ends with the thread, before completion"
	);
	assert_eq!(stop.shutdown_reason(), Some(SessionShutdownReason::UserStopped));
}

#[test]
fn initialization_failure_is_reported_and_stops_the_session() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let drops = Arc::new(AtomicUsize::new(0));
	let result = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		let drops = drops.clone();
		move |_ready: mpsc::SyncSender<()>| {
			let _owned = Owned(drops);
			Err("Failed to open render node /dev/dri/renderD128: No such device".to_string())
		}
	});
	assert_eq!(
		result,
		Err("Failed to open render node /dev/dri/renderD128: No such device".to_string())
	);
	assert_eq!(stop.shutdown_reason(), Some(REASON));
	completed(&stop);
	assert_eq!(drops.load(Ordering::SeqCst), 1);
	assert_eq!(tx.context_owners(), 2);
}

#[test]
fn panic_during_initialization_is_reported_and_releases_everything() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let drops = Arc::new(AtomicUsize::new(0));
	let result = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		let drops = drops.clone();
		move |_ready: mpsc::SyncSender<()>| -> Result<(), String> {
			let _owned = Owned(drops);
			panic!("mock backend panicked during initialization");
		}
	});
	assert_eq!(result, Err("the compositor stopped before it was ready".to_string()));
	assert_eq!(stop.shutdown_reason(), Some(REASON));
	completed(&stop);
	assert_eq!(drops.load(Ordering::SeqCst), 1, "unwinding releases backend resources");
	assert_eq!(tx.context_owners(), 2);
}

#[test]
fn stop_during_initialization_cancels_the_launch() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let drops = Arc::new(AtomicUsize::new(0));
	let canceller = std::thread::spawn({
		let stop = stop.clone();
		move || {
			std::thread::sleep(Duration::from_millis(50));
			stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
		}
	});
	let started = Instant::now();
	let result = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		let (stop, drops) = (stop.clone(), drops.clone());
		move |_ready: mpsc::SyncSender<()>| {
			let _owned = Owned(drops);
			// A backend still initializing returns on stop instead of reporting ready.
			wait_until_stopped(&stop);
			Ok(())
		}
	});
	canceller.join().unwrap();
	assert_eq!(result, Err("the compositor stopped before it was ready".to_string()));
	assert!(
		started.elapsed() < READY,
		"cancellation does not wait out the ready timeout"
	);
	assert_eq!(stop.shutdown_reason(), Some(SessionShutdownReason::UserStopped));
	completed(&stop);
	assert_eq!(drops.load(Ordering::SeqCst), 1);
	assert_eq!(tx.context_owners(), 2);
}

#[test]
fn ready_timeout_fails_the_launch_and_teardown_still_waits_for_the_thread() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let exited = Arc::new(AtomicBool::new(false));
	let result = start_capture_thread(
		"compositor",
		&stop,
		REASON,
		tx.context_hold(),
		Duration::from_secs(1),
		{
			let (stop, exited) = (stop.clone(), exited.clone());
			move |_ready: mpsc::SyncSender<()>| {
				wait_until_stopped(&stop);
				std::thread::sleep(Duration::from_millis(50));
				exited.store(true, Ordering::SeqCst);
				Ok(())
			}
		},
	);
	assert_eq!(
		result,
		Err("the compositor did not become ready within 1 seconds".to_string())
	);
	// The manager's teardown of the failed launch stops the session.
	assert!(!stop.is_shutdown_triggered());
	stop.trigger_shutdown(SessionShutdownReason::TransitionFailed).unwrap();
	completed(&stop);
	assert!(
		exited.load(Ordering::SeqCst),
		"completion waited for the backend thread"
	);
	assert_eq!(tx.context_owners(), 2);
}

#[test]
fn backend_exit_after_readiness_stops_the_session() {
	let stop = ShutdownManager::new();
	let (tx, _rx) = capture_channel();
	let (exit_tx, exit_rx) = mpsc::channel::<()>();
	let ready = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		move |ready: mpsc::SyncSender<()>| {
			ready.send(()).unwrap();
			// The backend's own loop ends (lost GPU, render failure, crash).
			let _ = exit_rx.recv();
			Err("Event loop dispatch error: device lost".to_string())
		}
	});
	assert_eq!(ready, Ok(()));
	assert!(!stop.is_shutdown_triggered());
	exit_tx.send(()).unwrap();
	completed(&stop);
	assert_eq!(stop.shutdown_reason(), Some(REASON));
	assert_eq!(tx.context_owners(), 2);
}

#[test]
fn completed_session_refuses_to_start_a_backend() {
	let stop = ShutdownManager::new();
	stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	completed(&stop);
	let (tx, _rx) = capture_channel();
	let ran = Arc::new(AtomicBool::new(false));
	let result = start_capture_thread("compositor", &stop, REASON, tx.context_hold(), READY, {
		let ran = ran.clone();
		move |_ready: mpsc::SyncSender<()>| {
			ran.store(true, Ordering::SeqCst);
			Ok(())
		}
	});
	assert_eq!(
		result,
		Err("the session stopped before the compositor started".to_string())
	);
	assert!(
		!ran.load(Ordering::SeqCst),
		"no backend thread runs for a finished session"
	);
	assert_eq!(tx.context_owners(), 2);
}

/// A frame the backend published before it exited stays readable by the
/// pipeline: the backend's teardown cannot close the descriptors of a frame
/// still queued or being imported.
#[test]
fn frame_published_before_backend_exit_outlives_the_backend() {
	let stop = ShutdownManager::new();
	let (tx, rx) = capture_channel();
	let context = tx.context_hold();
	let (identity_tx, identity_rx) = mpsc::channel();
	let ready = start_capture_thread("compositor", &stop, REASON, context, READY, {
		let stop = stop.clone();
		move |ready: mpsc::SyncSender<()>| {
			let source = dmabuf();
			identity_tx.send(identity(source.handles().next().unwrap())).unwrap();
			ready.send(()).unwrap();
			let credit = loop {
				if let Some(credit) = tx.try_acquire() {
					break credit;
				}
				std::thread::sleep(Duration::from_millis(1));
			};
			let consumed = Arc::new(AtomicBool::new(false));
			let frame = ExportedFrame::from_dmabuf(&source, 0, consumed, FrameColorSpace::Srgb, None);
			tx.try_send(frame, credit).map_err(|error| format!("{error:?}"))?;
			// The backend drops its own buffer and sender, then waits for the stop.
			drop(source);
			drop(tx);
			wait_until_stopped(&stop);
			Ok(())
		}
	});
	assert_eq!(ready, Ok(()));
	let expected = identity_rx.recv().unwrap();
	let frame = rx.recv_timeout(Duration::from_secs(5)).expect("frame");
	stop.trigger_shutdown(SessionShutdownReason::UserStopped).unwrap();
	completed(&stop);
	assert_eq!(identity(frame.planes().next().unwrap().fd), expected);
	assert!(
		!frame.consumed.load(Ordering::Acquire),
		"only the consumer releases content"
	);
	assert!(
		frame.capture_credit.is_some(),
		"demand stays occupied while the frame is held"
	);
}
