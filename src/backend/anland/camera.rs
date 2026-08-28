//! Rust PipeWire bridge for the Anland camera service.
//!
//! The public functions intentionally mirror the former C entry points. All PipeWire
//! objects live on one worker thread; callers and per-camera socket readers communicate
//! with that thread through PipeWire channels.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{self, Cursor, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use spa::buffer::DataType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::param::ParamType;
use spa::pod::serialize::PodSerializer;
use spa::pod::{self, ChoiceValue, Pod, Property};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};

const MAX_CAMERAS: usize = 8;
const CAMERA_SLOTS: u8 = 2;
const MAX_DIMENSION: u32 = 8192;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_STREAM_DRAIN_BATCH: usize = 32;
const MAX_INFO_PAYLOAD: usize = 1 + MAX_CAMERAS * 4;
const INFO_TIMEOUT: Duration = Duration::from_millis(300);
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const CAMERA_FPS: u32 = 30;

const CTRL_GET_INFO: u8 = 0x01;
const CTRL_START_RECORD: u8 = 0x02;
const CTRL_STOP_RECORD: u8 = 0x03;
const CTRL_INFO_REPLY: u8 = 0x81;

const STREAM_GET_SHM: u8 = 1;
const STREAM_SHM_OFFER: u8 = 2;
const STREAM_READY: u8 = 3;
const STREAM_DONE: u8 = 4;

const FORMAT_I420: u16 = 0;
const FORMAT_NV12: u16 = 1;
const FORMAT_NV21: u16 = 2;

static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Engine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

struct Engine {
    sender: pw::channel::Sender<Command>,
    worker: JoinHandle<()>,
}

enum Command {
    SetResources(Resources),
    Clear,
    Shutdown,
}

struct Resources {
    ctrl: Option<OwnedFd>,
    streams: Vec<Option<OwnedFd>>,
    initial_sizes: Vec<(u32, u32)>,
}

enum WorkerEvent {
    Streaming {
        camera: usize,
        node_generation: u64,
        streaming: bool,
    },
    Frame {
        camera: usize,
    },
    ReaderClosed {
        camera: usize,
        stream_generation: u64,
    },
    NodeFailed {
        camera: usize,
        node_generation: u64,
    },
    PipeWireBroken(u64),
}

/// Starts the camera worker. Idempotent.
///
/// A missing PipeWire server is not a startup failure: the worker reconnects once per
/// second, matching the C bridge. `-1` is returned only when the worker/loop itself could
/// not be created.
pub fn start() -> i32 {
    let mut global = engine()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if global.is_some() {
        return 0;
    }

    let (sender, receiver) = pw::channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = match thread::Builder::new()
        .name("anland-camera".to_owned())
        .spawn(move || worker_main(receiver, ready_tx))
    {
        Ok(worker) => worker,
        Err(err) => {
            eprintln!("anland-camera: failed to spawn worker: {err}");
            return -1;
        }
    };

    match ready_rx.recv() {
        Ok(Ok(())) => {
            *global = Some(Engine { sender, worker });
            0
        }
        Ok(Err(err)) => {
            eprintln!("anland-camera: worker setup failed: {err}");
            let _ = worker.join();
            -1
        }
        Err(_) => {
            eprintln!("anland-camera: worker exited during setup");
            let _ = worker.join();
            -1
        }
    }
}

/// Stops the worker and destroys all nodes and adopted resources. Idempotent.
pub fn stop() {
    // Keep lifecycle operations serialized until the old worker is gone, so a racing
    // start() cannot create a second PipeWire worker during teardown.
    let mut global = engine()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(current) = global.take() {
        let _ = current.sender.send(Command::Shutdown);
        let _ = current.worker.join();
    }
}

/// Adopts a control FD and `num_cameras` per-camera stream FDs.
///
/// Ownership is taken immediately, including FDs beyond the eight-camera publication
/// limit. Excess FDs are closed. On any startup or channel error every adopted FD is
/// closed by its `OwnedFd`.
///
/// # Safety
///
/// When `num_cameras > 0`, `stream_fds` must point to at least `num_cameras` readable
/// `i32` values. Every nonnegative descriptor must be open and exclusively transferred
/// to this function.
pub unsafe fn set_resources(ctrl_fd: i32, stream_fds: *const i32, num_cameras: i32) {
    let count = num_cameras.max(0) as usize;
    let mut seen = HashSet::with_capacity(count.min(MAX_CAMERAS).saturating_add(1));
    let ctrl = adopt_fd(ctrl_fd, &mut seen);
    let mut streams = Vec::with_capacity(count.min(MAX_CAMERAS));

    if count != 0 && stream_fds.is_null() {
        eprintln!("anland-camera: null stream_fds for {count} cameras");
        return;
    }

    for index in 0..count {
        let fd = unsafe { *stream_fds.add(index) };
        let owned = adopt_fd(fd, &mut seen);
        if index < MAX_CAMERAS {
            streams.push(owned);
        }
        // Descriptors past MAX_CAMERAS are dropped here and therefore closed.
    }

    let initial_sizes = ctrl
        .as_ref()
        .map(|fd| query_initial_sizes(fd.as_raw_fd(), streams.len()))
        .unwrap_or_else(|| vec![(0, 0); streams.len()]);
    let resources = Resources {
        ctrl,
        streams,
        initial_sizes,
    };

    if start() != 0 {
        return;
    }

    let sender = {
        let global = engine()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        global.as_ref().map(|engine| engine.sender.clone())
    };
    if let Some(sender) = sender {
        let _ = sender.send(Command::SetResources(resources));
    }
}

/// Detaches the consumer while retaining published camera nodes and blank-frame output.
pub fn clear() {
    let sender = {
        let global = engine()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        global.as_ref().map(|engine| engine.sender.clone())
    };
    if let Some(sender) = sender {
        let _ = sender.send(Command::Clear);
    }
}

unsafe fn adopt_fd(fd: RawFd, seen: &mut HashSet<RawFd>) -> Option<OwnedFd> {
    if fd < 0 || !seen.insert(fd) {
        return None;
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if !set_cloexec(owned.as_raw_fd()) {
        eprintln!("anland-camera: failed to set FD_CLOEXEC on fd {fd}");
    }
    Some(owned)
}

fn set_cloexec(fd: RawFd) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0
}

fn worker_main(
    command_receiver: pw::channel::Receiver<Command>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(value) => value,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(value) => value,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let (event_sender, event_receiver) = pw::channel::channel();
    let state = Rc::new(RefCell::new(WorkerState::new(context, event_sender)));

    let command_state = Rc::clone(&state);
    let quit_loop = mainloop.clone();
    let _commands = command_receiver.attach(mainloop.loop_(), move |command| {
        let shutdown = matches!(command, Command::Shutdown);
        command_state.borrow_mut().handle_command(command);
        if shutdown {
            quit_loop.quit();
        }
    });

    let event_state = Rc::clone(&state);
    let _events = event_receiver.attach(mainloop.loop_(), move |event| {
        event_state.borrow_mut().handle_event(event);
    });

    let timer_state = Rc::downgrade(&state);
    let reconnect_timer = mainloop.loop_().add_timer(move |_| {
        if let Some(state) = timer_state.upgrade() {
            state.borrow_mut().maintain_pipewire();
        }
    });
    let _ =
        reconnect_timer.update_timer(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)));

    state.borrow_mut().maintain_pipewire();
    if ready.send(Ok(())).is_err() {
        state.borrow_mut().shutdown();
        return;
    }

    mainloop.run();
    state.borrow_mut().shutdown();
}

struct WorkerState {
    context: pw::context::ContextRc,
    events: pw::channel::Sender<WorkerEvent>,
    core: Option<CoreConnection>,
    core_generation: u64,
    ctrl: Option<OwnedFd>,
    ctrl_generation: u64,
    active_cameras: usize,
    cameras: Vec<Camera>,
    stopping: bool,
}

impl WorkerState {
    fn new(context: pw::context::ContextRc, events: pw::channel::Sender<WorkerEvent>) -> Self {
        let cameras = (0..MAX_CAMERAS).map(Camera::new).collect();
        Self {
            context,
            events,
            core: None,
            core_generation: 0,
            ctrl: None,
            ctrl_generation: 0,
            active_cameras: 0,
            cameras,
            stopping: false,
        }
    }

    fn handle_command(&mut self, command: Command) {
        if self.stopping {
            return;
        }
        match command {
            Command::SetResources(resources) => self.set_resources(resources),
            Command::Clear => self.clear_resources(),
            Command::Shutdown => self.shutdown(),
        }
    }

    fn handle_event(&mut self, event: WorkerEvent) {
        if self.stopping {
            return;
        }
        match event {
            WorkerEvent::Streaming {
                camera,
                node_generation,
                streaming,
            } => {
                let Some(camera_state) = self.cameras.get(camera) else {
                    return;
                };
                let mut data = camera_state.data.borrow_mut();
                if data.node_generation != node_generation || data.streaming == streaming {
                    return;
                }
                data.streaming = streaming;
                drop(data);
                self.reconcile_recording(camera);
            }
            WorkerEvent::Frame { camera } => self.handle_frame(camera),
            WorkerEvent::ReaderClosed {
                camera,
                stream_generation,
            } => {
                let Some(camera_state) = self.cameras.get(camera) else {
                    return;
                };
                let mut data = camera_state.data.borrow_mut();
                if data.stream_generation != stream_generation {
                    return;
                }
                data.live = false;
                data.have_frame = false;
                drop(data);
                self.reconcile_recording(camera);
            }
            WorkerEvent::NodeFailed {
                camera,
                node_generation,
            } => {
                let Some(camera_state) = self.cameras.get(camera) else {
                    return;
                };
                if camera_state.data.borrow().node_generation != node_generation {
                    return;
                }
                self.destroy_node(camera);
            }
            WorkerEvent::PipeWireBroken(generation) => {
                if generation == self.core_generation {
                    eprintln!("anland-camera: PipeWire core disconnected");
                    self.drop_pipewire();
                }
            }
        }
    }

    fn set_resources(&mut self, mut resources: Resources) {
        // Stop the old consumer before swapping channels. Nodes stay in place.
        for index in 0..self.active_cameras {
            self.stop_reader(index);
            self.reconcile_recording(index);
        }
        self.ctrl = resources.ctrl.take();
        self.ctrl_generation = self.ctrl_generation.wrapping_add(1);

        let new_count = resources.streams.len().min(MAX_CAMERAS);
        for index in new_count..self.active_cameras {
            self.destroy_node(index);
        }
        self.active_cameras = new_count;

        for index in 0..new_count {
            let size = resources
                .initial_sizes
                .get(index)
                .copied()
                .unwrap_or_default();
            if self.cameras[index].node.is_none() && frame_size(size.0, size.1).is_some() {
                let mut data = self.cameras[index].data.borrow_mut();
                data.width = size.0;
                data.height = size.1;
            }
            self.create_node(index);
            self.attach_reader(index, resources.streams[index].take());
        }
    }

    fn clear_resources(&mut self) {
        for index in 0..self.active_cameras {
            self.stop_reader(index);
            self.reconcile_recording(index);
        }
        self.ctrl = None;
        self.ctrl_generation = self.ctrl_generation.wrapping_add(1);
    }

    fn shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;

        for index in 0..MAX_CAMERAS {
            self.stop_reader(index);
            self.reconcile_recording(index);
        }
        self.ctrl = None;
        self.drop_pipewire();
        for camera in &mut self.cameras {
            camera.reader = None;
            camera.node = None;
        }
    }

    fn maintain_pipewire(&mut self) {
        if self.stopping {
            return;
        }
        if self.core.is_none() {
            self.core_generation = self.core_generation.wrapping_add(1);
            let generation = self.core_generation;
            match self.context.connect_rc(None) {
                Ok(core) => {
                    let events = self.events.clone();
                    let listener = core
                        .add_listener_local()
                        .error(move |id, _seq, result, _message| {
                            if id == pw::core::PW_ID_CORE && result == -libc::EPIPE {
                                let _ = events.send(WorkerEvent::PipeWireBroken(generation));
                            }
                        })
                        .register();
                    self.core = Some(CoreConnection {
                        _listener: listener,
                        core,
                    });
                    eprintln!("anland-camera: connected to PipeWire");
                }
                Err(err) => {
                    eprintln!("anland-camera: PipeWire connect failed: {err}");
                }
            }
        }

        for index in 0..self.active_cameras {
            self.create_node(index);
            self.reconcile_recording(index);
        }
    }

    fn drop_pipewire(&mut self) {
        for index in 0..MAX_CAMERAS {
            self.destroy_node(index);
        }
        self.core = None;
    }

    fn create_node(&mut self, index: usize) {
        if index >= self.active_cameras || self.cameras[index].node.is_some() {
            return;
        }
        let Some(core) = self.core.as_ref().map(|connection| connection.core.clone()) else {
            return;
        };

        let (width, height, format, generation, callbacks_data) = {
            let camera = &self.cameras[index];
            let mut data = camera.data.borrow_mut();
            if data.width == 0 || data.height == 0 {
                data.width = DEFAULT_WIDTH;
                data.height = DEFAULT_HEIGHT;
            }
            data.streaming = false;
            data.process_seen = false;
            data.node_generation = data.node_generation.wrapping_add(1);
            (
                data.width,
                data.height,
                data.format,
                data.node_generation,
                Rc::clone(&camera.data),
            )
        };

        let name = format!("anland-camera-{index}");
        let description = format!("Anland remote camera {index}");
        let properties = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_ROLE => "Camera",
            *pw::keys::NODE_NAME => name.clone(),
            *pw::keys::NODE_DESCRIPTION => description,
        };
        let stream = match pw::stream::StreamRc::new(core, &name, properties) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("anland-camera: cam={index} stream creation failed: {err}");
                return;
            }
        };

        let callbacks = NodeCallbacks {
            camera: index,
            node_generation: generation,
            data: callbacks_data,
            events: self.events.clone(),
        };
        let listener = match stream
            .add_local_listener_with_user_data(callbacks)
            .state_changed(|_, callbacks, old, new| {
                eprintln!(
                    "anland-camera: cam={} state {old:?} -> {new:?}",
                    callbacks.camera
                );
                let streaming = new == pw::stream::StreamState::Streaming;
                let failed = matches!(new, pw::stream::StreamState::Error(_));
                let _ = callbacks.events.send(WorkerEvent::Streaming {
                    camera: callbacks.camera,
                    node_generation: callbacks.node_generation,
                    streaming,
                });
                if failed {
                    let _ = callbacks.events.send(WorkerEvent::NodeFailed {
                        camera: callbacks.camera,
                        node_generation: callbacks.node_generation,
                    });
                }
            })
            .param_changed(|stream, callbacks, id, param| {
                node_param_changed(stream, callbacks, id, param);
            })
            .process(|stream, callbacks| {
                node_process(stream, callbacks);
            })
            .register()
        {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("anland-camera: cam={index} listener creation failed: {err}");
                return;
            }
        };

        let format_bytes = match make_format_pod(width, height, format) {
            Some(bytes) => bytes,
            None => return,
        };
        let Some(format) = Pod::from_bytes(&format_bytes) else {
            return;
        };
        let mut params = [format];
        if let Err(err) = stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        ) {
            eprintln!("anland-camera: cam={index} connect failed: {err}");
            return;
        }

        self.cameras[index].node = Some(Node {
            _listener: listener,
            stream,
        });
    }

    fn destroy_node(&mut self, index: usize) {
        if index >= self.cameras.len() {
            return;
        }
        {
            let mut data = self.cameras[index].data.borrow_mut();
            data.node_generation = data.node_generation.wrapping_add(1);
            data.streaming = false;
        }
        self.reconcile_recording(index);
        self.cameras[index].node = None;
    }

    fn attach_reader(&mut self, index: usize, stream: Option<OwnedFd>) {
        self.stop_reader(index);
        let Some(stream) = stream else {
            self.reconcile_recording(index);
            return;
        };

        let generation = {
            let mut data = self.cameras[index].data.borrow_mut();
            data.live = true;
            data.have_frame = false;
            data.stream_generation
        };
        match ReaderHandle::spawn(
            index,
            generation,
            stream,
            Arc::clone(&self.cameras[index].mailbox),
            self.events.clone(),
        ) {
            Ok(reader) => self.cameras[index].reader = Some(reader),
            Err(err) => {
                eprintln!("anland-camera: cam={index} reader creation failed: {err}");
                self.cameras[index].data.borrow_mut().live = false;
            }
        }
        self.reconcile_recording(index);
    }

    fn stop_reader(&mut self, index: usize) {
        if index >= self.cameras.len() {
            return;
        }
        {
            let mut data = self.cameras[index].data.borrow_mut();
            data.stream_generation = data.stream_generation.wrapping_add(1);
            data.live = false;
            data.have_frame = false;
        }
        if let Some(reader) = self.cameras[index].reader.take() {
            reader.stop();
        }
        *self.cameras[index]
            .mailbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn reconcile_recording(&mut self, index: usize) {
        if index >= self.cameras.len() {
            return;
        }
        let (want, recording, width, height) = {
            let data = self.cameras[index].data.borrow();
            (
                data.streaming && data.live && self.ctrl.is_some(),
                data.recording && data.recording_generation == self.ctrl_generation,
                data.width,
                data.height,
            )
        };
        if want == recording {
            if want {
                if let Some(reader) = &self.cameras[index].reader {
                    reader.request_shm();
                }
            }
            return;
        }

        let sent = if want {
            let Some(_) = frame_size(width, height) else {
                return;
            };
            let mut payload = [0u8; 5];
            payload[0] = index as u8;
            payload[1..3].copy_from_slice(&(width as u16).to_le_bytes());
            payload[3..5].copy_from_slice(&(height as u16).to_le_bytes());
            self.ctrl
                .as_ref()
                .is_some_and(|ctrl| send_control(ctrl.as_raw_fd(), CTRL_START_RECORD, &payload))
        } else {
            self.ctrl.as_ref().is_some_and(|ctrl| {
                send_control(ctrl.as_raw_fd(), CTRL_STOP_RECORD, &[index as u8])
            })
        };
        if !sent {
            return;
        }

        {
            let mut data = self.cameras[index].data.borrow_mut();
            data.recording = want;
            data.recording_generation = self.ctrl_generation;
        }
        if want {
            eprintln!("anland-camera: START_RECORD cam={index} {width}x{height}");
            if let Some(reader) = &self.cameras[index].reader {
                reader.request_shm();
            }
        } else {
            eprintln!("anland-camera: STOP_RECORD cam={index}");
        }
    }

    fn handle_frame(&mut self, index: usize) {
        let Some(camera) = self.cameras.get_mut(index) else {
            return;
        };
        let frame = camera
            .mailbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(frame) = frame else {
            return;
        };
        let Some(expected) = frame_size(frame.width, frame.height) else {
            return;
        };
        if !is_camera_format(frame.format) || frame.pixels.len() != expected {
            return;
        }

        let (changed, old_width, old_height, old_format, generation) = {
            let data = camera.data.borrow();
            if data.stream_generation != frame.stream_generation || !data.live {
                return;
            }
            (
                frame.width != data.width
                    || frame.height != data.height
                    || frame.format != data.format,
                data.width,
                data.height,
                data.format,
                data.node_generation,
            )
        };

        if changed {
            let Some(node) = camera.node.as_ref() else {
                return;
            };
            eprintln!(
                "anland-camera: cam={index} renegotiate {old_width}x{old_height}/fmt{old_format} -> {}x{}/fmt{}",
                frame.width, frame.height, frame.format
            );
            let updated =
                make_format_pod(frame.width, frame.height, frame.format).is_some_and(|bytes| {
                    let Some(pod) = Pod::from_bytes(&bytes) else {
                        return false;
                    };
                    let mut params = [pod];
                    match node.stream.update_params(&mut params) {
                        Ok(()) => true,
                        Err(err) => {
                            eprintln!("anland-camera: cam={index} format update failed: {err}");
                            false
                        }
                    }
                });
            if !updated {
                let _ = self.events.send(WorkerEvent::NodeFailed {
                    camera: index,
                    node_generation: generation,
                });
                return;
            }
        }

        let mut data = camera.data.borrow_mut();
        if data.stream_generation != frame.stream_generation || !data.live {
            return;
        }
        if !data.have_frame {
            eprintln!(
                "anland-camera: cam={index} first frame {}x{}",
                frame.width, frame.height
            );
        }
        data.width = frame.width;
        data.height = frame.height;
        data.format = frame.format;
        data.frame = frame.pixels;
        data.have_frame = true;
    }
}

struct CoreConnection {
    // Listener must be dropped before the core it is registered on.
    _listener: pw::core::Listener,
    core: pw::core::CoreRc,
}

struct Camera {
    data: Rc<RefCell<CameraData>>,
    mailbox: Arc<Mutex<Option<ReaderFrame>>>,
    node: Option<Node>,
    reader: Option<ReaderHandle>,
}

impl Camera {
    fn new(index: usize) -> Self {
        Self {
            data: Rc::new(RefCell::new(CameraData {
                index,
                width: 0,
                height: 0,
                format: FORMAT_NV21,
                streaming: false,
                recording: false,
                recording_generation: 0,
                live: false,
                process_seen: false,
                have_frame: false,
                frame: Vec::new(),
                stream_generation: 0,
                node_generation: 0,
            })),
            mailbox: Arc::new(Mutex::new(None)),
            node: None,
            reader: None,
        }
    }
}

struct CameraData {
    index: usize,
    width: u32,
    height: u32,
    format: u16,
    streaming: bool,
    recording: bool,
    recording_generation: u64,
    live: bool,
    process_seen: bool,
    have_frame: bool,
    frame: Vec<u8>,
    stream_generation: u64,
    node_generation: u64,
}

struct Node {
    // Listener must be dropped before the stream it is registered on.
    _listener: pw::stream::StreamListener<NodeCallbacks>,
    stream: pw::stream::StreamRc,
}

struct NodeCallbacks {
    camera: usize,
    node_generation: u64,
    data: Rc<RefCell<CameraData>>,
    events: pw::channel::Sender<WorkerEvent>,
}

fn node_process(stream: &pw::stream::Stream, callbacks: &mut NodeCallbacks) {
    let mut buffer = match stream.dequeue_buffer() {
        Some(buffer) => buffer,
        None => {
            let mut data = callbacks.data.borrow_mut();
            if !data.process_seen {
                eprintln!(
                    "anland-camera: cam={} on_process called, buffer=NULL(out of buffers)",
                    data.index
                );
                data.process_seen = true;
            }
            return;
        }
    };

    let mut data = callbacks.data.borrow_mut();
    if !data.process_seen {
        eprintln!(
            "anland-camera: cam={} on_process called, buffer=ok",
            data.index
        );
        data.process_seen = true;
    }
    let Some(output) = buffer.datas_mut().first_mut() else {
        return;
    };
    let live = data.live && data.have_frame;
    let Some(expected) = frame_size(data.width, data.height) else {
        *output.chunk_mut().size_mut() = 0;
        return;
    };
    if live && data.frame.len() != expected {
        *output.chunk_mut().size_mut() = 0;
        return;
    }
    let Some(bytes) = output.data() else {
        return;
    };
    if bytes.len() < expected {
        *output.chunk_mut().size_mut() = 0;
        return;
    }
    if live {
        bytes[..expected].copy_from_slice(&data.frame);
    } else {
        bytes[..expected].fill(128);
    }
    let chunk = output.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = data.width as i32;
    *chunk.size_mut() = expected as u32;
}

fn node_param_changed(
    stream: &pw::stream::Stream,
    callbacks: &mut NodeCallbacks,
    id: u32,
    param: Option<&Pod>,
) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        return;
    };
    let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
        return;
    };
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    let mut raw = VideoInfoRaw::new();
    if raw.parse(param).is_err() {
        return;
    }
    let size = raw.size();
    let Some(format) = camera_format(raw.format()) else {
        return;
    };
    let (current_width, current_height) = {
        let data = callbacks.data.borrow();
        (data.width, data.height)
    };
    let width = if size.width == 0 {
        current_width
    } else {
        size.width
    };
    let height = if size.height == 0 {
        current_height
    } else {
        size.height
    };
    let Some(bytes) = frame_size(width, height) else {
        return;
    };
    eprintln!(
        "anland-camera: cam={} format negotiated {width}x{height} ({bytes}b), declaring buffers",
        callbacks.camera
    );

    let updated = make_buffer_pod(width, height).is_some_and(|buffer_pod| {
        let Some(pod) = Pod::from_bytes(&buffer_pod) else {
            return false;
        };
        let mut params = [pod];
        match stream.update_params(&mut params) {
            Ok(()) => true,
            Err(err) => {
                eprintln!(
                    "anland-camera: cam={} buffer update failed: {err}",
                    callbacks.camera
                );
                false
            }
        }
    });
    if !updated {
        let _ = callbacks.events.send(WorkerEvent::NodeFailed {
            camera: callbacks.camera,
            node_generation: callbacks.node_generation,
        });
        return;
    }

    let mut data = callbacks.data.borrow_mut();
    data.width = width;
    data.height = height;
    data.format = format;
}

fn video_format(format: u16) -> VideoFormat {
    match format {
        FORMAT_NV12 => VideoFormat::NV12,
        FORMAT_NV21 => VideoFormat::NV21,
        _ => VideoFormat::I420,
    }
}

fn camera_format(format: VideoFormat) -> Option<u16> {
    if format == VideoFormat::I420 {
        Some(FORMAT_I420)
    } else if format == VideoFormat::NV12 {
        Some(FORMAT_NV12)
    } else if format == VideoFormat::NV21 {
        Some(FORMAT_NV21)
    } else {
        None
    }
}

fn make_format_pod(width: u32, height: u32, format: u16) -> Option<Vec<u8>> {
    frame_size(width, height)?;
    if !is_camera_format(format) {
        return None;
    }
    let object = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, video_format(format)),
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: CAMERA_FPS,
                denom: 1
            }
        ),
    );
    serialize_object(object)
}

fn make_buffer_pod(width: u32, height: u32) -> Option<Vec<u8>> {
    let size = frame_size(width, height)?.min(i32::MAX as usize) as i32;
    let stride = width.min(i32::MAX as u32) as i32;
    let mem_ptr = 1i32.checked_shl(DataType::MemPtr.as_raw())?;
    let mem_fd = 1i32.checked_shl(DataType::MemFd.as_raw())?;
    let data_types = mem_ptr | mem_fd;
    let object = pod::object!(
        SpaTypes::ObjectParamBuffers,
        ParamType::Buffers,
        Property::new(
            spa::sys::SPA_PARAM_BUFFERS_buffers,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: 4,
                    min: 2,
                    max: 8,
                },
            ))),
        ),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, pod::Value::Int(1)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_size, pod::Value::Int(size)),
        Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, pod::Value::Int(stride)),
        Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: data_types,
                    flags: Vec::new(),
                },
            ))),
        ),
    );
    serialize_object(object)
}

fn serialize_object(object: pod::Object) -> Option<Vec<u8>> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(object))
        .ok()
        .map(|result| result.0.into_inner())
}

fn frame_size(width: u32, height: u32) -> Option<usize> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
    {
        return None;
    }
    let pixels = (width as usize).checked_mul(height as usize)?;
    let bytes = pixels.checked_mul(3)?.checked_div(2)?;
    (bytes <= MAX_FRAME_BYTES).then_some(bytes)
}

fn is_camera_format(format: u16) -> bool {
    matches!(format, FORMAT_I420 | FORMAT_NV12 | FORMAT_NV21)
}

enum ReaderCommand {
    RequestShm,
    Stop,
}

struct ReaderFrame {
    stream_generation: u64,
    width: u32,
    height: u32,
    format: u16,
    pixels: Vec<u8>,
}

struct ReaderHandle {
    commands: mpsc::Sender<ReaderCommand>,
    wake: UnixStream,
    worker: JoinHandle<()>,
}

impl ReaderHandle {
    fn spawn(
        camera: usize,
        generation: u64,
        stream: OwnedFd,
        mailbox: Arc<Mutex<Option<ReaderFrame>>>,
        events: pw::channel::Sender<WorkerEvent>,
    ) -> io::Result<Self> {
        let (wake, wake_reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_reader.set_nonblocking(true)?;
        let (commands, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("anland-camera-{camera}-reader"))
            .spawn(move || {
                reader_main(
                    camera,
                    generation,
                    stream,
                    wake_reader,
                    receiver,
                    mailbox,
                    events,
                )
            })?;
        Ok(Self {
            commands,
            wake,
            worker,
        })
    }

    fn request_shm(&self) {
        if self.commands.send(ReaderCommand::RequestShm).is_ok() {
            let _ = (&self.wake).write(&[1]);
        }
    }

    fn stop(self) {
        let _ = self.commands.send(ReaderCommand::Stop);
        let _ = (&self.wake).write(&[1]);
        let _ = self.worker.join();
    }
}

fn reader_main(
    camera: usize,
    generation: u64,
    stream: OwnedFd,
    mut wake: UnixStream,
    commands: mpsc::Receiver<ReaderCommand>,
    mailbox: Arc<Mutex<Option<ReaderFrame>>>,
    events: pw::channel::Sender<WorkerEvent>,
) {
    let mut mapping: Option<Mapping> = None;
    let mut needs_shm = true;
    let mut shm_requested = false;
    let mut stop = false;

    while !stop {
        stop = handle_reader_commands(&commands, stream.as_raw_fd(), needs_shm, &mut shm_requested);
        if stop {
            break;
        }

        let mut fds = [
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0u8; 64];
            loop {
                match wake.read(&mut bytes) {
                    Ok(0) => {
                        stop = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        stop = true;
                        break;
                    }
                }
            }
            stop |= handle_reader_commands(
                &commands,
                stream.as_raw_fd(),
                needs_shm,
                &mut shm_requested,
            );
        }
        if stop {
            break;
        }

        if fds[1].revents & libc::POLLIN != 0 {
            for _ in 0..MAX_STREAM_DRAIN_BATCH {
                match receive_stream_message(stream.as_raw_fd()) {
                    ReceiveResult::Message(message, mut received_fds) => match message.kind {
                        STREAM_SHM_OFFER => {
                            let replacement = if received_fds.is_empty() {
                                None
                            } else {
                                Mapping::new(received_fds.remove(0), message.a as usize)
                            };
                            shm_requested = false;
                            if let Some(replacement) = replacement {
                                eprintln!(
                                    "anland-camera: cam={camera} shm ready {} B/slot",
                                    replacement.slot_bytes
                                );
                                mapping = Some(replacement);
                                needs_shm = false;
                            } else {
                                eprintln!("anland-camera: cam={camera} rejected shm offer");
                                needs_shm = true;
                            }
                        }
                        STREAM_READY => {
                            let pixels = if is_camera_format(message.format) {
                                frame_size(message.a, message.b).and_then(|bytes| {
                                    mapping
                                        .as_ref()
                                        .and_then(|mapping| mapping.copy_slot(message.slot, bytes))
                                })
                            } else {
                                None
                            };
                            // Always release a valid or invalid slot so malformed input cannot
                            // wedge the producer's double buffer.
                            let _ = send_stream(stream.as_raw_fd(), STREAM_DONE, message.slot);
                            if let Some(pixels) = pixels {
                                let should_notify = {
                                    let mut latest = mailbox
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    let should_notify = latest.is_none();
                                    *latest = Some(ReaderFrame {
                                        stream_generation: generation,
                                        width: message.a,
                                        height: message.b,
                                        format: message.format,
                                        pixels,
                                    });
                                    should_notify
                                };
                                if should_notify {
                                    let _ = events.send(WorkerEvent::Frame { camera });
                                }
                            } else {
                                needs_shm = true;
                                shm_requested = false;
                            }
                        }
                        _ => {}
                    },
                    ReceiveResult::Again => break,
                    ReceiveResult::Closed | ReceiveResult::Error => {
                        stop = true;
                        break;
                    }
                }
                stop |= handle_reader_commands(
                    &commands,
                    stream.as_raw_fd(),
                    needs_shm,
                    &mut shm_requested,
                );
                if stop {
                    break;
                }
            }
        }
        if fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
    }

    drop(mapping);
    let _ = events.send(WorkerEvent::ReaderClosed {
        camera,
        stream_generation: generation,
    });
}

fn handle_reader_commands(
    commands: &mpsc::Receiver<ReaderCommand>,
    stream: RawFd,
    needs_shm: bool,
    shm_requested: &mut bool,
) -> bool {
    while let Ok(command) = commands.try_recv() {
        match command {
            ReaderCommand::RequestShm => {
                if needs_shm && !*shm_requested {
                    *shm_requested = send_stream(stream, STREAM_GET_SHM, 0);
                }
            }
            ReaderCommand::Stop => return true,
        }
    }
    false
}

#[derive(Clone, Copy)]
struct StreamMessage {
    kind: u8,
    slot: u8,
    format: u16,
    a: u32,
    b: u32,
}

impl StreamMessage {
    fn from_bytes(bytes: &[u8; 12]) -> Self {
        Self {
            kind: bytes[0],
            slot: bytes[1],
            format: u16::from_le_bytes([bytes[2], bytes[3]]),
            a: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            b: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        }
    }
}

enum ReceiveResult {
    Message(StreamMessage, Vec<OwnedFd>),
    Again,
    Closed,
    Error,
}

fn receive_stream_message(fd: RawFd) -> ReceiveResult {
    let mut bytes = [0u8; 12];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // usize alignment is sufficient for cmsghdr and leaves room for several unexpected
    // rights, all of which must be adopted and closed.
    let mut control = [0usize; 8];
    let mut header: libc::msghdr = unsafe { mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = mem::size_of_val(&control);

    let received =
        unsafe { libc::recvmsg(fd, &mut header, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
    if received == 0 {
        return ReceiveResult::Closed;
    }
    if received < 0 {
        let error = io::Error::last_os_error();
        return if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            ReceiveResult::Again
        } else {
            ReceiveResult::Error
        };
    }

    let mut rights = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET
                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                && (*cmsg).cmsg_len >= libc::CMSG_LEN(mem::size_of::<RawFd>() as _) as usize
            {
                let header_len = libc::CMSG_LEN(0) as usize;
                let payload_len = (*cmsg).cmsg_len.saturating_sub(header_len);
                let count = payload_len / mem::size_of::<RawFd>();
                let descriptors = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    let received_fd = *descriptors.add(index);
                    if received_fd >= 0 {
                        let owned = OwnedFd::from_raw_fd(received_fd);
                        if !set_cloexec(owned.as_raw_fd()) {
                            eprintln!(
                                "anland-camera: failed to set FD_CLOEXEC on received fd {received_fd}"
                            );
                        }
                        rights.push(owned);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }

    if received as usize != bytes.len()
        || header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
    {
        return ReceiveResult::Again;
    }
    ReceiveResult::Message(StreamMessage::from_bytes(&bytes), rights)
}

struct Mapping {
    len: usize,
    slot_bytes: usize,
    fd: OwnedFd,
}

impl Mapping {
    fn new(fd: OwnedFd, slot_bytes: usize) -> Option<Self> {
        if slot_bytes == 0 || slot_bytes > MAX_FRAME_BYTES {
            return None;
        }
        let len = slot_bytes.checked_mul(CAMERA_SLOTS as usize)?;

        let mut stat = mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_size < 0 || usize::try_from(stat.st_size).ok()? < len {
            return None;
        }

        Some(Self {
            len,
            slot_bytes,
            fd,
        })
    }

    fn copy_slot(&self, slot: u8, requested: usize) -> Option<Vec<u8>> {
        if slot >= CAMERA_SLOTS || requested == 0 || requested > self.slot_bytes {
            return None;
        }
        let offset = (slot as usize).checked_mul(self.slot_bytes)?;
        if offset.checked_add(requested)? > self.len {
            return None;
        }
        let mut output = Vec::new();
        output.try_reserve_exact(requested).ok()?;
        output.resize(requested, 0);

        let mut read = 0usize;
        while read < requested {
            let position = offset.checked_add(read)?;
            let position = libc::off_t::try_from(position).ok()?;
            let result = unsafe {
                libc::pread(
                    self.fd.as_raw_fd(),
                    output[read..].as_mut_ptr().cast(),
                    requested - read,
                    position,
                )
            };
            if result == 0 {
                return None;
            }
            if result < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            read = read.checked_add(result as usize)?;
        }
        Some(output)
    }
}

fn send_control(fd: RawFd, kind: u8, payload: &[u8]) -> bool {
    let Ok(length) = u16::try_from(payload.len()) else {
        return false;
    };
    let mut message = Vec::with_capacity(4 + payload.len());
    message.push(kind);
    message.push(0);
    message.extend_from_slice(&length.to_le_bytes());
    message.extend_from_slice(payload);
    send_exact(fd, &message)
}

fn send_stream(fd: RawFd, kind: u8, slot: u8) -> bool {
    let mut message = [0u8; 12];
    message[0] = kind;
    message[1] = slot;
    send_exact(fd, &message)
}

fn send_exact(fd: RawFd, message: &[u8]) -> bool {
    unsafe {
        libc::send(
            fd,
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        ) == message.len() as isize
    }
}

fn query_initial_sizes(fd: RawFd, count: usize) -> Vec<(u32, u32)> {
    let mut sizes = vec![(0, 0); count];
    if !send_control(fd, CTRL_GET_INFO, &[]) {
        return sizes;
    }
    let deadline = Instant::now() + INFO_TIMEOUT;
    let Some(socket_type) = socket_type(fd) else {
        return sizes;
    };
    let reply = if socket_type == libc::SOCK_STREAM {
        receive_info_stream(fd, deadline)
    } else if socket_type == libc::SOCK_SEQPACKET {
        receive_info_packet(fd, deadline)
    } else {
        None
    };
    let Some(reply) = reply else {
        return sizes;
    };

    let cameras = reply[4] as usize;
    for (index, size) in sizes.iter_mut().enumerate().take(cameras.min(count)) {
        let offset = 5 + index * 4;
        let width = u16::from_le_bytes([reply[offset], reply[offset + 1]]) as u32;
        let height = u16::from_le_bytes([reply[offset + 2], reply[offset + 3]]) as u32;
        if frame_size(width, height).is_some() {
            *size = (width, height);
        }
    }
    sizes
}

fn socket_type(fd: RawFd) -> Option<libc::c_int> {
    let mut value = 0;
    let mut length = mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    (result == 0 && length as usize == mem::size_of_val(&value)).then_some(value)
}

fn receive_info_stream(fd: RawFd, deadline: Instant) -> Option<Vec<u8>> {
    let mut reply = [0u8; 4 + MAX_INFO_PAYLOAD];
    let header_len = 4;
    loop {
        let received = unsafe {
            libc::recv(
                fd,
                reply.as_mut_ptr().cast(),
                header_len,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if received == 0 {
            return None;
        }
        if received < 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return None;
            }
        } else if received as usize == header_len {
            break;
        }
        if !wait_for_info_data(fd, deadline) {
            return None;
        }
    }

    let declared = u16::from_le_bytes([reply[2], reply[3]]) as usize;
    let total = header_len.checked_add(declared)?;
    if !wait_for_info_message(fd, total, deadline) {
        return None;
    }
    if !(1..=MAX_INFO_PAYLOAD).contains(&declared) {
        discard_exact_nonblocking(fd, total, deadline)?;
        return None;
    }

    let received = unsafe {
        libc::recv(
            fd,
            reply.as_mut_ptr().cast(),
            total,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if received != total as isize {
        return None;
    }
    let valid = reply[0] == CTRL_INFO_REPLY
        && validate_info_payload(&reply[header_len..total], declared).is_some();
    recv_exact_nonblocking(fd, &mut reply[..total], deadline)?;
    valid.then(|| reply[..total].to_vec())
}

fn receive_info_packet(fd: RawFd, deadline: Instant) -> Option<Vec<u8>> {
    let mut reply = [0u8; 4 + MAX_INFO_PAYLOAD];
    let packet_len = loop {
        let received = unsafe {
            libc::recv(
                fd,
                reply.as_mut_ptr().cast(),
                reply.len(),
                libc::MSG_PEEK | libc::MSG_TRUNC | libc::MSG_DONTWAIT,
            )
        };
        if received == 0 {
            return None;
        }
        if received < 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return None;
            }
            if !wait_for_info_data(fd, deadline) {
                return None;
            }
        } else {
            break received as usize;
        }
    };
    if packet_len < 4 || packet_len > reply.len() {
        discard_info_packet(fd, &mut reply);
        return None;
    }
    let declared = match validate_info_header(&reply[..4]) {
        Some(declared) => declared,
        None => {
            discard_info_packet(fd, &mut reply);
            return None;
        }
    };
    let total = 4usize.checked_add(declared)?;
    if packet_len != total || validate_info_payload(&reply[4..total], declared).is_none() {
        discard_info_packet(fd, &mut reply);
        return None;
    }
    let received = unsafe { libc::recv(fd, reply.as_mut_ptr().cast(), total, libc::MSG_DONTWAIT) };
    if received != total as isize {
        return None;
    }
    Some(reply[..total].to_vec())
}

fn validate_info_header(header: &[u8]) -> Option<usize> {
    (header.first() == Some(&CTRL_INFO_REPLY))
        .then(|| declared_info_payload(header))
        .flatten()
}

fn declared_info_payload(header: &[u8]) -> Option<usize> {
    if header.len() != 4 {
        return None;
    }
    let declared = u16::from_le_bytes([header[2], header[3]]) as usize;
    (1..=MAX_INFO_PAYLOAD)
        .contains(&declared)
        .then_some(declared)
}

fn validate_info_payload(payload: &[u8], declared: usize) -> Option<()> {
    if payload.len() != declared {
        return None;
    }
    let cameras = *payload.first()? as usize;
    if cameras > MAX_CAMERAS || declared != 1 + cameras * 4 {
        return None;
    }
    Some(())
}

fn wait_for_info_data(fd: RawFd, deadline: Instant) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    let remaining = deadline.saturating_duration_since(now);
    let timeout = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, timeout.max(1)) };
    if result < 0 {
        return io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            && Instant::now() < deadline;
    }
    if result == 0 || pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return false;
    }
    // POLLIN remains asserted for a partial stream message. Yield briefly rather than
    // spinning while the peer supplies the tail.
    thread::sleep(Duration::from_millis(1));
    Instant::now() < deadline
}

fn wait_for_info_message(fd: RawFd, total: usize, deadline: Instant) -> bool {
    loop {
        let mut available: libc::c_int = 0;
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } != 0 {
            return false;
        }
        if available >= 0 && available as usize >= total {
            return true;
        }
        if !wait_for_info_data(fd, deadline) {
            return false;
        }
    }
}

fn recv_exact_nonblocking(fd: RawFd, output: &mut [u8], deadline: Instant) -> Option<()> {
    let mut offset = 0;
    while offset < output.len() {
        let received = unsafe {
            libc::recv(
                fd,
                output[offset..].as_mut_ptr().cast(),
                output.len() - offset,
                libc::MSG_DONTWAIT,
            )
        };
        if received == 0 {
            return None;
        }
        if received < 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) || !wait_for_info_data(fd, deadline)
            {
                return None;
            }
            continue;
        }
        offset += received as usize;
    }
    Some(())
}

fn discard_exact_nonblocking(fd: RawFd, mut remaining: usize, deadline: Instant) -> Option<()> {
    let mut scratch = [0u8; 256];
    while remaining != 0 {
        let bytes = remaining.min(scratch.len());
        recv_exact_nonblocking(fd, &mut scratch[..bytes], deadline)?;
        remaining -= bytes;
    }
    Some(())
}

fn discard_info_packet(fd: RawFd, scratch: &mut [u8]) {
    unsafe {
        libc::recv(
            fd,
            scratch.as_mut_ptr().cast(),
            scratch.len(),
            libc::MSG_DONTWAIT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_rejects_invalid_and_oversized_frames() {
        assert_eq!(frame_size(1280, 720), Some(1_382_400));
        assert_eq!(frame_size(0, 720), None);
        assert_eq!(frame_size(1281, 720), None);
        assert_eq!(frame_size(MAX_DIMENSION + 1, 720), None);
        assert_eq!(frame_size(MAX_DIMENSION, MAX_DIMENSION), None);
        assert_eq!(
            frame_size(MAX_DIMENSION, MAX_DIMENSION / 2),
            Some(50_331_648)
        );
    }

    #[test]
    fn info_layout_validation_is_strict_and_bounded() {
        assert_eq!(validate_info_header(&[CTRL_INFO_REPLY, 0, 5, 0]), Some(5));
        assert_eq!(validate_info_payload(&[1, 0, 5, 208, 2], 5), Some(()));
        assert_eq!(validate_info_header(&[0, 0, 5, 0]), None);
        assert_eq!(validate_info_header(&[CTRL_INFO_REPLY, 0, 0, 0]), None);
        assert_eq!(validate_info_payload(&[2, 0, 5, 208, 2], 5), None);
        assert_eq!(validate_info_payload(&[MAX_CAMERAS as u8 + 1], 1), None);
    }

    #[test]
    fn mapping_requires_two_slots_and_reads_exact_slot() {
        let short = make_memfd(15, &[]);
        assert!(Mapping::new(short, 8).is_none());

        let bytes = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let fd = make_memfd(bytes.len(), &bytes);
        let mapping = Mapping::new(fd, 8).expect("valid two-slot backing");
        assert_eq!(mapping.copy_slot(1, 8).as_deref(), Some(&bytes[8..]));
        assert!(mapping.copy_slot(CAMERA_SLOTS, 8).is_none());
        assert!(mapping.copy_slot(0, 9).is_none());
    }

    fn make_memfd(len: usize, contents: &[u8]) -> OwnedFd {
        let name = b"anland-camera-test\0";
        let raw = unsafe { libc::memfd_create(name.as_ptr().cast(), libc::MFD_CLOEXEC) };
        assert!(
            raw >= 0,
            "memfd_create failed: {}",
            io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) },
            0,
            "ftruncate failed: {}",
            io::Error::last_os_error()
        );
        if !contents.is_empty() {
            assert_eq!(
                unsafe {
                    libc::pwrite(fd.as_raw_fd(), contents.as_ptr().cast(), contents.len(), 0)
                },
                contents.len() as isize,
                "pwrite failed: {}",
                io::Error::last_os_error()
            );
        }
        fd
    }
}
