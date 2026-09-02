//! Rust PipeWire bridge for Anland playback and microphone audio.
//!
//! PipeWire objects are confined to one worker thread. Public lifecycle calls and the
//! consumer socket reader communicate with that thread through PipeWire channels.

use std::cell::RefCell;
use std::ffi::CString;
use std::io::{self, Cursor, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use spa::param::ParamType;
use spa::pod::serialize::PodSerializer;
use spa::pod::{self, Pod};
use spa::utils::SpaTypes;

const DEFAULT_RATE: u32 = 48_000;
const DEFAULT_PLAY_CHANNELS: u32 = 2;
const DEFAULT_CAP_CHANNELS: u32 = 1;
const MIC_RING_BYTES: usize = 48_000 * 2 * mem::size_of::<i16>();
const MAX_DGRAM: usize = 64 * 1024;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

const AUDIO_MSG_FORMAT: u32 = 1;
const AUDIO_MSG_PCM: u32 = 2;
const AUDIO_FORMAT_S16LE: u32 = 0;
const AUDIO_ROLE_PLAYBACK: u32 = 0;
const AUDIO_ROLE_CAPTURE: u32 = 1;
const AUDIO_HEADER_BYTES: usize = 8;
const AUDIO_FORMAT_BYTES: usize = 20;

static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();

fn engine() -> &'static Mutex<Option<Engine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

struct Engine {
    sender: pw::channel::Sender<Command>,
    worker: JoinHandle<()>,
}

enum Command {
    SetSocket(Option<OwnedFd>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FormatAnnouncement {
    rate: u32,
    channels: u32,
    format: u32,
    role: u32,
    quantum: u32,
}

enum WorkerEvent {
    Format {
        socket_generation: u64,
        format: FormatAnnouncement,
        applied: mpsc::SyncSender<()>,
    },
    ReaderClosed(u64),
    PipeWireBroken(u64),
}

/// Starts the audio worker. Idempotent.
///
/// A missing PipeWire server is not a startup failure; the worker retries once per
/// second. `-1` is returned only if the worker or its main loop cannot be created.
pub fn start() -> i32 {
    catch_unwind(AssertUnwindSafe(start_inner)).unwrap_or(-1)
}

fn start_inner() -> i32 {
    let mut global = engine()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if global
        .as_ref()
        .is_some_and(|current| current.worker.is_finished())
    {
        if let Some(stale) = global.take() {
            let _ = stale.worker.join();
        }
    }
    if global.is_some() {
        return 0;
    }

    let (sender, receiver) = pw::channel::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let panic_ready = ready_tx.clone();
    let worker = match thread::Builder::new()
        .name("anland-audio".to_owned())
        .spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| worker_main(receiver, ready_tx))).is_err() {
                let _ = panic_ready.try_send(Err("audio worker panicked".to_owned()));
                eprintln!("anland-audio: worker panicked");
            }
        }) {
        Ok(worker) => worker,
        Err(err) => {
            eprintln!("anland-audio: failed to spawn worker: {err}");
            return -1;
        }
    };

    match ready_rx.recv() {
        Ok(Ok(())) => {
            *global = Some(Engine { sender, worker });
            0
        }
        Ok(Err(err)) => {
            eprintln!("anland-audio: worker setup failed: {err}");
            let _ = worker.join();
            -1
        }
        Err(_) => {
            eprintln!("anland-audio: worker exited during setup");
            let _ = worker.join();
            -1
        }
    }
}

/// Stops the audio worker and releases the duplicated consumer socket. Idempotent.
pub fn stop() {
    let _ = catch_unwind(AssertUnwindSafe(stop_inner));
}

fn stop_inner() {
    // Keep lifecycle operations serialized until the old worker is gone. This prevents a
    // racing start() from publishing a second worker during teardown.
    let mut global = engine()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(current) = global.take() {
        let _ = current.sender.send(Command::Shutdown);
        let _ = current.worker.join();
    }
}

/// Replaces the consumer audio socket with an owned close-on-exec duplicate.
///
/// A negative or invalid descriptor detaches the current consumer. The caller retains
/// ownership of the descriptor passed here.
pub fn set_fd(fd: i32) {
    let _ = catch_unwind(AssertUnwindSafe(|| set_fd_inner(fd)));
}

fn set_fd_inner(fd: RawFd) {
    let global = engine()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(current) = global.as_ref() else {
        return;
    };

    // Match the C bridge: replacing the socket always detaches the previous one first;
    // therefore a failed duplication is sent as SetSocket(None).
    let owned = duplicate_fd(fd);
    let _ = current.sender.send(Command::SetSocket(owned));
}

fn duplicate_fd(fd: RawFd) -> Option<OwnedFd> {
    if fd < 0 {
        return None;
    }
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        None
    } else {
        Some(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
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
    let _ = reconnect_timer.update_timer(Some(RECONNECT_INTERVAL), Some(RECONNECT_INTERVAL));

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
    streams: Option<AudioStreams>,
    core_generation: u64,
    socket_generation: u64,
    socket: Rc<RefCell<Option<Arc<OwnedFd>>>>,
    reader: Option<ReaderHandle>,
    ring: Arc<Mutex<ByteRing>>,
    capture_stride: Arc<AtomicU32>,
    playback_format: CurrentFormat,
    capture_format: CurrentFormat,
    desired_playback_format: CurrentFormat,
    desired_capture_format: CurrentFormat,
    stopping: bool,
}

impl WorkerState {
    fn new(context: pw::context::ContextRc, events: pw::channel::Sender<WorkerEvent>) -> Self {
        Self {
            context,
            events,
            core: None,
            streams: None,
            core_generation: 0,
            socket_generation: 0,
            socket: Rc::new(RefCell::new(None)),
            reader: None,
            ring: Arc::new(Mutex::new(ByteRing::new(MIC_RING_BYTES))),
            capture_stride: Arc::new(AtomicU32::new(
                DEFAULT_CAP_CHANNELS * mem::size_of::<i16>() as u32,
            )),
            playback_format: CurrentFormat::new(DEFAULT_RATE, DEFAULT_PLAY_CHANNELS),
            capture_format: CurrentFormat::new(DEFAULT_RATE, DEFAULT_CAP_CHANNELS),
            desired_playback_format: CurrentFormat::new(DEFAULT_RATE, DEFAULT_PLAY_CHANNELS),
            desired_capture_format: CurrentFormat::new(DEFAULT_RATE, DEFAULT_CAP_CHANNELS),
            stopping: false,
        }
    }

    fn handle_command(&mut self, command: Command) {
        if self.stopping {
            return;
        }
        match command {
            Command::SetSocket(socket) => self.set_socket(socket),
            Command::Shutdown => self.shutdown(),
        }
    }

    fn handle_event(&mut self, event: WorkerEvent) {
        if self.stopping {
            return;
        }
        match event {
            WorkerEvent::Format {
                socket_generation,
                format,
                applied,
            } if socket_generation == self.socket_generation && self.reader.is_some() => {
                self.apply_format(format);
                let _ = applied.send(());
            }
            WorkerEvent::ReaderClosed(generation) if generation == self.socket_generation => {
                self.detach_socket();
            }
            WorkerEvent::PipeWireBroken(generation) if generation == self.core_generation => {
                eprintln!("anland-audio: PipeWire connection or stream failed");
                self.rebuild_pipewire();
            }
            _ => {}
        }
    }

    fn set_socket(&mut self, socket: Option<OwnedFd>) {
        self.detach_socket();
        let Some(socket) = socket else {
            return;
        };

        let socket = Arc::new(socket);
        let generation = self.socket_generation;
        match ReaderHandle::spawn(
            generation,
            Arc::clone(&socket),
            Arc::clone(&self.ring),
            Arc::clone(&self.capture_stride),
            self.events.clone(),
        ) {
            Ok(reader) => {
                *self.socket.borrow_mut() = Some(socket);
                self.reader = Some(reader);
            }
            Err(err) => {
                eprintln!("anland-audio: failed to spawn socket reader: {err}");
            }
        }
    }

    fn detach_socket(&mut self) {
        self.socket_generation = self.socket_generation.wrapping_add(1);
        if let Some(reader) = self.reader.take() {
            reader.stop();
        }
        *self.socket.borrow_mut() = None;
        lock_ring(&self.ring).reset();
    }

    fn apply_format(&mut self, announcement: FormatAnnouncement) {
        let playback = match (announcement.format, announcement.role) {
            (AUDIO_FORMAT_S16LE, AUDIO_ROLE_PLAYBACK) => true,
            (AUDIO_FORMAT_S16LE, AUDIO_ROLE_CAPTURE) => false,
            _ => return,
        };
        let default_channels = if playback {
            DEFAULT_PLAY_CHANNELS
        } else {
            DEFAULT_CAP_CHANNELS
        };
        let rate = if announcement.rate == 0 {
            DEFAULT_RATE
        } else {
            announcement.rate
        };
        let channels = if announcement.channels == 0 {
            default_channels
        } else {
            announcement.channels
        };

        // AudioInfoRaw serializes rate as a signed SPA Int and has a fixed channel map.
        // Ignore malformed announcements rather than allowing a callback panic or invalid POD.
        let Some(stride) = frame_stride(channels) else {
            eprintln!("anland-audio: ignoring invalid format rate={rate} channels={channels}");
            return;
        };
        if rate > i32::MAX as u32 || channels as usize > MAX_CHANNELS {
            eprintln!("anland-audio: ignoring invalid format rate={rate} channels={channels}");
            return;
        }

        let next = CurrentFormat {
            rate,
            channels,
            quantum: announcement.quantum,
        };
        let current = if playback {
            self.desired_playback_format = next;
            self.playback_format
        } else {
            self.desired_capture_format = next;
            self.capture_format
        };
        let format_changed = current.rate != rate || current.channels != channels;
        let quantum_changed = current.quantum != announcement.quantum;
        if !format_changed && !quantum_changed {
            return;
        }

        let Some(streams) = &self.streams else {
            self.maintain_pipewire();
            return;
        };
        let stream = if playback {
            &streams.speaker.stream
        } else {
            &streams.microphone.stream
        };

        if format_changed {
            if let Err(err) = update_stream_format(stream, rate, channels) {
                eprintln!("anland-audio: format update failed: {err}");
                self.rebuild_pipewire();
                return;
            }
        }
        update_latency(stream, announcement.quantum, rate);

        if playback {
            self.playback_format = next;
        } else {
            if format_changed {
                let mut ring = lock_ring(&self.ring);
                self.capture_stride.store(stride, Ordering::Release);
                ring.reset();
            }
            self.capture_format = next;
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
                    eprintln!("anland-audio: connected to PipeWire");
                }
                Err(err) => {
                    eprintln!("anland-audio: PipeWire connect failed: {err}");
                    return;
                }
            }
        }

        if self.streams.is_none() {
            let Some(core) = self.core.as_ref().map(|connection| connection.core.clone()) else {
                return;
            };
            match create_streams(
                core,
                self.desired_playback_format,
                self.desired_capture_format,
                Rc::clone(&self.socket),
                Arc::clone(&self.ring),
                Arc::clone(&self.capture_stride),
                self.events.clone(),
                self.core_generation,
            ) {
                Ok(streams) => {
                    let capture_changed = self.capture_format.rate
                        != self.desired_capture_format.rate
                        || self.capture_format.channels != self.desired_capture_format.channels;
                    self.streams = Some(streams);
                    self.playback_format = self.desired_playback_format;
                    if capture_changed {
                        let stride = frame_stride(self.desired_capture_format.channels)
                            .expect("validated capture format");
                        let mut ring = lock_ring(&self.ring);
                        self.capture_stride.store(stride, Ordering::Release);
                        ring.reset();
                    }
                    self.capture_format = self.desired_capture_format;
                }
                Err(err) => {
                    eprintln!("anland-audio: PipeWire stream setup failed: {err}");
                    self.drop_pipewire();
                }
            }
        }
    }

    fn rebuild_pipewire(&mut self) {
        self.drop_pipewire();
        self.maintain_pipewire();
    }

    fn drop_pipewire(&mut self) {
        self.streams = None;
        self.core = None;
    }

    fn shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.detach_socket();
        self.drop_pipewire();
    }
}

#[derive(Clone, Copy)]
struct CurrentFormat {
    rate: u32,
    channels: u32,
    quantum: u32,
}

impl CurrentFormat {
    fn new(rate: u32, channels: u32) -> Self {
        Self {
            rate,
            channels,
            quantum: 0,
        }
    }
}

struct CoreConnection {
    // The listener must be dropped before its core.
    _listener: pw::core::Listener,
    core: pw::core::CoreRc,
}

struct AudioStreams {
    speaker: AudioNode<PlaybackCallbacks>,
    microphone: AudioNode<CaptureCallbacks>,
}

struct AudioNode<T: 'static> {
    // The listener must be dropped before its stream.
    _listener: pw::stream::StreamListener<T>,
    stream: pw::stream::StreamRc,
}

struct PlaybackCallbacks {
    socket: Rc<RefCell<Option<Arc<OwnedFd>>>>,
}

struct CaptureCallbacks {
    ring: Arc<Mutex<ByteRing>>,
    stride: Arc<AtomicU32>,
}

#[allow(clippy::too_many_arguments)]
fn create_streams(
    core: pw::core::CoreRc,
    playback: CurrentFormat,
    capture: CurrentFormat,
    socket: Rc<RefCell<Option<Arc<OwnedFd>>>>,
    ring: Arc<Mutex<ByteRing>>,
    capture_stride: Arc<AtomicU32>,
    events: pw::channel::Sender<WorkerEvent>,
    core_generation: u64,
) -> Result<AudioStreams, String> {
    let speaker = pw::stream::StreamRc::new(
        core.clone(),
        "anland-speaker",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "anland-speaker",
            *pw::keys::NODE_DESCRIPTION => "Anland remote speaker",
            *pw::keys::PRIORITY_SESSION => "1010",
            *pw::keys::PRIORITY_DRIVER => "1010",
        },
    )
    .map_err(|err| err.to_string())?;
    let speaker_events = events.clone();
    let speaker_listener = speaker
        .add_local_listener_with_user_data(PlaybackCallbacks { socket })
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!("anland-audio: speaker stream failed: {err}");
                let _ = speaker_events.send(WorkerEvent::PipeWireBroken(core_generation));
            }
        })
        .process(playback_process)
        .register()
        .map_err(|err| err.to_string())?;
    connect_stream(
        &speaker,
        spa::utils::Direction::Input,
        playback.rate,
        playback.channels,
        playback.quantum,
    )?;
    let speaker = AudioNode {
        _listener: speaker_listener,
        stream: speaker,
    };

    let microphone = pw::stream::StreamRc::new(
        core,
        "anland-mic",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => "anland-mic",
            *pw::keys::NODE_DESCRIPTION => "Anland remote microphone",
            *pw::keys::PRIORITY_SESSION => "1010",
            *pw::keys::PRIORITY_DRIVER => "1010",
        },
    )
    .map_err(|err| err.to_string())?;
    let microphone_listener = microphone
        .add_local_listener_with_user_data(CaptureCallbacks {
            ring,
            stride: capture_stride,
        })
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(err) = state {
                eprintln!("anland-audio: microphone stream failed: {err}");
                let _ = events.send(WorkerEvent::PipeWireBroken(core_generation));
            }
        })
        .process(capture_process)
        .register()
        .map_err(|err| err.to_string())?;
    connect_stream(
        &microphone,
        spa::utils::Direction::Output,
        capture.rate,
        capture.channels,
        capture.quantum,
    )?;
    let microphone = AudioNode {
        _listener: microphone_listener,
        stream: microphone,
    };

    Ok(AudioStreams {
        speaker,
        microphone,
    })
}

fn connect_stream(
    stream: &pw::stream::Stream,
    direction: spa::utils::Direction,
    rate: u32,
    channels: u32,
    quantum: u32,
) -> Result<(), String> {
    update_latency(stream, quantum, rate);
    let bytes = make_format_pod(rate, channels).ok_or("failed to serialize audio format")?;
    let format = Pod::from_bytes(&bytes).ok_or("invalid serialized audio format")?;
    let mut params = [format];
    stream
        .connect(
            direction,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|err| err.to_string())
}

fn update_stream_format(
    stream: &pw::stream::Stream,
    rate: u32,
    channels: u32,
) -> Result<(), String> {
    let bytes = make_format_pod(rate, channels).ok_or("failed to serialize audio format")?;
    let format = Pod::from_bytes(&bytes).ok_or("invalid serialized audio format")?;
    let mut params = [format];
    stream
        .update_params(&mut params)
        .map_err(|err| err.to_string())
}

fn make_format_pod(rate: u32, channels: u32) -> Option<Vec<u8>> {
    if rate > i32::MAX as u32 || channels == 0 || channels as usize > MAX_CHANNELS {
        return None;
    }

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(rate);
    info.set_channels(channels);
    let mut position = [0; MAX_CHANNELS];
    if channels >= 2 {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    } else {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    }
    info.set_position(position);

    let object = pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(object))
        .ok()
        .map(|serialized| serialized.0.into_inner())
}

fn update_latency(stream: &pw::stream::Stream, quantum: u32, rate: u32) {
    let value = (quantum != 0)
        .then(|| CString::new(format!("{quantum}/{rate}")).expect("latency contains no null byte"));
    let item = spa::sys::spa_dict_item {
        key: pw::sys::PW_KEY_NODE_LATENCY.as_ptr().cast(),
        value: value.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
    };
    let properties = spa::sys::spa_dict {
        flags: 0,
        n_items: 1,
        items: &item,
    };
    // pipewire 0.10 has no high-level Stream::update_properties wrapper. PipeWire
    // copies this one borrowed item; a null value removes node.latency for quantum 0.
    unsafe {
        pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), &properties);
    }
}

fn playback_process(stream: &pw::stream::Stream, callbacks: &mut PlaybackCallbacks) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };

    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let Some(bytes) = data.data() else {
        return;
    };
    let Some(end) = offset.checked_add(size) else {
        return;
    };
    let Some(payload) = bytes.get(offset..end) else {
        return;
    };
    if payload.is_empty() {
        return; // matches the C bridge: an empty period is not a PCM message
    }
    let socket = callbacks.socket.borrow().clone();
    if let Some(socket) = socket {
        send_pcm(socket.as_raw_fd(), payload);
    }
}

fn capture_process(stream: &pw::stream::Stream, callbacks: &mut CaptureCallbacks) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested();
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };

    let (written, stride) = {
        let Some(output) = data.data() else {
            return;
        };
        // Serialize the stride load with a successful capture-format commit and its
        // ring reset, so no callback can drain new-format PCM using the old stride.
        let mut ring = lock_ring(&callbacks.ring);
        let stride = callbacks.stride.load(Ordering::Acquire) as usize;
        if stride == 0 {
            return;
        }
        let available_frames = output.len() / stride;
        let requested_frames = usize::try_from(requested).unwrap_or(usize::MAX);
        let frames = if requested == 0 {
            available_frames
        } else {
            available_frames.min(requested_frames)
        };
        let bytes = frames * stride;
        let got = ring.read(&mut output[..bytes]);
        output[got..bytes].fill(0);
        (bytes, stride)
    };

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride.min(i32::MAX as usize) as i32;
    *chunk.size_mut() = written.min(u32::MAX as usize) as u32;
}

fn send_pcm(fd: RawFd, payload: &[u8]) {
    let Ok(size) = u32::try_from(payload.len()) else {
        return;
    };
    let mut header = [0u8; AUDIO_HEADER_BYTES];
    header[..4].copy_from_slice(&AUDIO_MSG_PCM.to_le_bytes());
    header[4..].copy_from_slice(&size.to_le_bytes());

    let mut iov = [
        libc::iovec {
            iov_base: header.as_ptr().cast_mut().cast(),
            iov_len: header.len(),
        },
        libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        },
    ];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = iov.len();
    unsafe {
        libc::sendmsg(fd, &message, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL);
    }
}

struct ReaderHandle {
    stop: Arc<AtomicBool>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl ReaderHandle {
    fn spawn(
        generation: u64,
        socket: Arc<OwnedFd>,
        ring: Arc<Mutex<ByteRing>>,
        capture_stride: Arc<AtomicU32>,
        events: pw::channel::Sender<WorkerEvent>,
    ) -> io::Result<Self> {
        let (wake, wake_reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_reader.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("anland-audio-reader".to_owned())
            .spawn(move || {
                reader_main(
                    generation,
                    socket,
                    ring,
                    capture_stride,
                    events,
                    reader_stop,
                    wake_reader,
                )
            })?;
        Ok(Self {
            stop,
            wake,
            worker: Some(worker),
        })
    }

    fn stop(mut self) {
        self.cancel_and_join();
    }

    fn cancel_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.write(&[1]);
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

fn reader_main(
    generation: u64,
    socket: Arc<OwnedFd>,
    ring: Arc<Mutex<ByteRing>>,
    capture_stride: Arc<AtomicU32>,
    events: pw::channel::Sender<WorkerEvent>,
    stop: Arc<AtomicBool>,
    mut wake: UnixStream,
) {
    let mut receive_buffer = [0u8; MAX_DGRAM];

    while !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.as_raw_fd(),
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

        if fds[0].revents != 0 {
            let mut bytes = [0u8; 64];
            loop {
                match wake.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
        }

        if fds[1].revents & libc::POLLIN != 0 {
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let received = unsafe {
                    libc::recv(
                        socket.as_raw_fd(),
                        receive_buffer.as_mut_ptr().cast(),
                        receive_buffer.len(),
                        libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                    )
                };
                if received == 0 {
                    let _ = events.send(WorkerEvent::ReaderClosed(generation));
                    return;
                }
                if received < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    let _ = events.send(WorkerEvent::ReaderClosed(generation));
                    return;
                }
                let received = received as usize;
                if received > receive_buffer.len() {
                    continue;
                }
                parse_audio_datagram(
                    &receive_buffer[..received],
                    generation,
                    &ring,
                    &capture_stride,
                    &events,
                    &stop,
                );
            }
        }

        if fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
    }

    let _ = events.send(WorkerEvent::ReaderClosed(generation));
}

fn parse_audio_datagram(
    datagram: &[u8],
    generation: u64,
    ring: &Mutex<ByteRing>,
    capture_stride: &AtomicU32,
    events: &pw::channel::Sender<WorkerEvent>,
    stop: &AtomicBool,
) {
    let Some((message_type, payload)) = audio_datagram_payload(datagram) else {
        return;
    };

    match message_type {
        AUDIO_MSG_FORMAT => {
            let Some(format) = parse_format_announcement(payload) else {
                return;
            };
            let (applied, acknowledgment) = mpsc::sync_channel(0);
            if events
                .send(WorkerEvent::Format {
                    socket_generation: generation,
                    format,
                    applied,
                })
                .is_ok()
            {
                while !stop.load(Ordering::Acquire) {
                    match acknowledgment.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
        AUDIO_MSG_PCM => {
            // Serialize validation with format commits, which update the stride and reset
            // the ring under this same lock.
            let mut ring = lock_ring(ring);
            let stride = capture_stride.load(Ordering::Acquire) as usize;
            write_pcm_payload(&mut ring, stride, payload);
        }
        _ => {}
    }
}

fn audio_datagram_payload(datagram: &[u8]) -> Option<(u32, &[u8])> {
    let header = datagram.get(..AUDIO_HEADER_BYTES)?;
    let message_type = u32::from_le_bytes(header[..4].try_into().ok()?);
    let declared_size = u32::from_le_bytes(header[4..].try_into().ok()?) as usize;
    let payload = &datagram[AUDIO_HEADER_BYTES..];
    (declared_size == payload.len()).then_some((message_type, payload))
}

fn parse_format_announcement(payload: &[u8]) -> Option<FormatAnnouncement> {
    if payload.len() != AUDIO_FORMAT_BYTES {
        return None;
    }
    let format = FormatAnnouncement {
        rate: read_u32(payload, 0)?,
        channels: read_u32(payload, 4)?,
        format: read_u32(payload, 8)?,
        role: read_u32(payload, 12)?,
        quantum: read_u32(payload, 16)?,
    };
    if format.format != AUDIO_FORMAT_S16LE
        || !matches!(format.role, AUDIO_ROLE_PLAYBACK | AUDIO_ROLE_CAPTURE)
    {
        return None;
    }
    Some(format)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset + mem::size_of::<u32>())?;
    Some(u32::from_le_bytes(value.try_into().ok()?))
}

fn frame_stride(channels: u32) -> Option<u32> {
    channels
        .checked_mul(mem::size_of::<i16>() as u32)
        .filter(|&stride| stride != 0)
}

fn write_pcm_payload(ring: &mut ByteRing, stride: usize, mut payload: &[u8]) {
    if stride == 0 || !payload.len().is_multiple_of(stride) {
        return;
    }
    let capacity = ring.bytes.len() / stride * stride;
    if capacity == 0 {
        return;
    }
    if payload.len() > capacity {
        payload = &payload[payload.len() - capacity..];
    }
    if ring.fill + payload.len() > capacity {
        let drop = ring.fill + payload.len() - capacity;
        ring.tail = (ring.tail + drop) % ring.bytes.len();
        ring.fill -= drop;
    }
    ring.write(payload);
}

fn lock_ring(ring: &Mutex<ByteRing>) -> MutexGuard<'_, ByteRing> {
    ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ByteRing {
    bytes: Vec<u8>,
    head: usize,
    tail: usize,
    fill: usize,
}

impl ByteRing {
    fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
            head: 0,
            tail: 0,
            fill: 0,
        }
    }

    fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.fill = 0;
    }

    fn write(&mut self, mut input: &[u8]) {
        if self.bytes.is_empty() {
            return;
        }
        if input.len() > self.bytes.len() {
            input = &input[input.len() - self.bytes.len()..];
        }
        if self.fill + input.len() > self.bytes.len() {
            let drop = self.fill + input.len() - self.bytes.len();
            self.tail = (self.tail + drop) % self.bytes.len();
            self.fill -= drop;
        }

        let first = input.len().min(self.bytes.len() - self.head);
        self.bytes[self.head..self.head + first].copy_from_slice(&input[..first]);
        self.bytes[..input.len() - first].copy_from_slice(&input[first..]);
        self.head = (self.head + input.len()) % self.bytes.len();
        self.fill += input.len();
    }

    fn read(&mut self, output: &mut [u8]) -> usize {
        if self.bytes.is_empty() {
            return 0;
        }
        let got = output.len().min(self.fill);
        let first = got.min(self.bytes.len() - self.tail);
        output[..first].copy_from_slice(&self.bytes[self.tail..self.tail + first]);
        output[first..got].copy_from_slice(&self.bytes[..got - first]);
        self.tail = (self.tail + got) % self.bytes.len();
        self.fill -= got;
        got
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(message_type: u32, declared_size: usize, payload: &[u8]) -> Vec<u8> {
        let mut datagram = Vec::with_capacity(AUDIO_HEADER_BYTES + payload.len());
        datagram.extend_from_slice(&message_type.to_le_bytes());
        datagram.extend_from_slice(&(declared_size as u32).to_le_bytes());
        datagram.extend_from_slice(payload);
        datagram
    }

    fn format_payload(format: u32, role: u32) -> Vec<u8> {
        [DEFAULT_RATE, DEFAULT_CAP_CHANNELS, format, role, 240]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect()
    }

    #[test]
    fn datagram_requires_exact_declared_payload_size() {
        let payload = [1, 2, 3, 4];
        assert!(audio_datagram_payload(&datagram(AUDIO_MSG_PCM, 4, &payload)).is_some());
        assert!(audio_datagram_payload(&datagram(AUDIO_MSG_PCM, 3, &payload)).is_none());
        assert!(audio_datagram_payload(&datagram(AUDIO_MSG_PCM, 5, &payload)).is_none());
    }

    #[test]
    fn format_requires_exact_size_s16le_and_known_role() {
        let playback = format_payload(AUDIO_FORMAT_S16LE, AUDIO_ROLE_PLAYBACK);
        let capture = format_payload(AUDIO_FORMAT_S16LE, AUDIO_ROLE_CAPTURE);
        assert!(parse_format_announcement(&playback).is_some());
        assert!(parse_format_announcement(&capture).is_some());
        assert!(parse_format_announcement(&playback[..AUDIO_FORMAT_BYTES - 1]).is_none());
        assert!(parse_format_announcement(&[playback.as_slice(), &[0]].concat()).is_none());
        assert!(parse_format_announcement(&format_payload(1, AUDIO_ROLE_CAPTURE)).is_none());
        assert!(parse_format_announcement(&format_payload(AUDIO_FORMAT_S16LE, 2)).is_none());
    }

    #[test]
    fn pcm_requires_capture_frame_alignment() {
        let mut ring = ByteRing::new(16);
        write_pcm_payload(&mut ring, 4, &[1, 2, 3]);
        assert_eq!(ring.fill, 0);
        write_pcm_payload(&mut ring, 4, &[1, 2, 3, 4]);
        assert_eq!(ring.fill, 4);
    }

    #[test]
    fn ring_keeps_latest_bytes_and_wraps() {
        let mut ring = ByteRing::new(5);
        ring.write(&[1, 2, 3]);
        let mut first = [0; 2];
        assert_eq!(ring.read(&mut first), 2);
        assert_eq!(first, [1, 2]);

        ring.write(&[4, 5, 6, 7, 8, 9]);
        let mut latest = [0; 8];
        assert_eq!(ring.read(&mut latest), 5);
        assert_eq!(&latest[..5], &[5, 6, 7, 8, 9]);
    }

    #[test]
    fn ring_underrun_can_be_silence_padded() {
        let mut ring = ByteRing::new(8);
        ring.write(&[1, 2, 3]);
        let mut output = [9; 6];
        let got = ring.read(&mut output);
        output[got..].fill(0);
        assert_eq!(output, [1, 2, 3, 0, 0, 0]);
    }
}
