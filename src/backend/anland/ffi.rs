//! Anland display-producer IPC.
//!
//! Historically this was a C library (`libdisplay_producer` + `socket_utils`);
//! it has been ported to Rust. The public surface (function names, signatures,
//! and the `display_ctx` opaque handle) is preserved verbatim so the backend
//! code in `mod.rs` is unchanged. The audio/camera bridges (`anland_audio_*`,
//! `anland_camera_*`) remain C and are linked when `have_anland_audio` is set.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

pub const MAX_BUFS: usize = 8;

// protocol.h input event types
pub const INPUT_TYPE_TOUCH: u32 = 1;
pub const INPUT_TYPE_KEY: u32 = 2;
pub const INPUT_TYPE_POINTER_MOTION: u32 = 3;
pub const INPUT_TYPE_POINTER_BUTTON: u32 = 4;
pub const INPUT_TYPE_POINTER_AXIS: u32 = 5;
pub const INPUT_TYPE_TOUCH_FRAME: u32 = 6;
pub const INPUT_TYPE_DISPLAY_REFRESH: u32 = 7;
pub const INPUT_TYPE_CLIPBOARD: u32 = 8;
pub const INPUT_TYPE_TEXT_INPUT: u32 = 9;
pub const INPUT_TYPE_ACTION: u32 = 10;
pub const INPUT_TYPE_RESOURCE: u32 = 11;
pub const INPUT_TYPE_RESOURCE_INVALID: u32 = 12;

pub const INPUT_ACTION_DOWN: i32 = 0;
pub const INPUT_ACTION_UP: i32 = 1;
pub const INPUT_ACTION_MOVE: i32 = 2;

// protocol.h output event types (producer -> consumer)
pub const OUTPUT_TYPE_CLIPBOARD: u32 = 1;
pub const OUTPUT_TYPE_RESOURCES_REQUEST: u32 = 2;
pub const OUTPUT_TYPE_SET_CONSUMER_VAR: u32 = 3;

pub const SERVICE_TYPE_CAMERA: u32 = 1;

// protocol.h control/data message types (producer side)
const CTRL_MSG_PRODUCER_HELLO: u32 = 2;
const CTRL_MSG_SCREEN_INFO: u32 = 7;
const CTRL_MSG_PICKUP_FDS: u32 = 9;
const CTRL_MSG_FDS_READY: u32 = 10;
const DATA_MSG_BUFS_READY: u32 = 200;
const DATA_MSG_INPUT_EVENT: u32 = 102;
const DATA_MSG_OUTPUT_EVENT: u32 = 103;
const DATA_MSG_INPUT_EXTEND_FDS: u32 = 104;

const HANDSHAKE_TIMEOUT_MS: c_int = 100;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
struct ctrl_msg {
    type_: u32,
    size: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
struct data_msg {
    type_: u32,
    size: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
struct screen_info {
    width: u32,
    height: u32,
    format: u32,
    refresh: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct buf_info {
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub offset: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TouchEvent {
    pub action: i32,
    pub x: f32,
    pub y: f32,
    pub pointer_id: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub action: i32,
    pub keycode: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PointerMotionEvent {
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PointerButtonEvent {
    pub button: u32,
    pub pressed: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PointerAxisEvent {
    pub axis: u32,
    pub value: f32,
    pub discrete: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DisplayEvent {
    pub refresh_mhz: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SizeEvent {
    pub size: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputActionEvent {
    pub action: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ResourceEvent {
    pub type_: u32,
    pub fdnum: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union InputEventData {
    pub touch: TouchEvent,
    pub key: KeyEvent,
    pub pointer_motion: PointerMotionEvent,
    pub pointer_button: PointerButtonEvent,
    pub pointer_axis: PointerAxisEvent,
    pub display: DisplayEvent,
    pub clipboard: SizeEvent,
    pub text_input: SizeEvent,
    pub input_action: InputActionEvent,
    pub resource: ResourceEvent,
    pub padding: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub type_: u32,
    pub data: InputEventData,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResourceRequestEvent {
    pub type_: u32,
    pub args: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SetConsumerVarEvent {
    pub var: u32,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union OutputEventData {
    pub clipboard: SizeEvent,
    pub resources_request: ResourceRequestEvent,
    pub set_consumer_var: SetConsumerVarEvent,
    pub padding: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct OutputEvent {
    pub type_: u32,
    pub data: OutputEventData,
}

/// Opaque producer context. Held by `mod.rs` as a raw pointer; the fields are
/// private and only touched by the functions below.
pub struct display_ctx {
    ctrl_fd: c_int,
    data_fd: c_int,
    buf_ready_efd: c_int,
    fence_fd: c_int,
    shm_fd: c_int,
    audio_fd: c_int,
    pending_render_fence: c_int,
    shm_ptr: *mut u32,
    screen_w: u32,
    screen_h: u32,
    pixel_format: u32,
    refresh: u32,
    fallback: bool,
    dmabuf_fds: [c_int; MAX_BUFS],
    dmabuf_infos: [buf_info; MAX_BUFS],
    buf_count: c_int,
    fallback_cb: Option<unsafe extern "C" fn(*mut c_void)>,
    fallback_userdata: *mut c_void,
}

// ============================================================================
// socket_utils (was socket_utils.c)
// ============================================================================

unsafe fn send_all(fd: c_int, buf: *const u8, len: usize) -> c_int {
    let mut off: usize = 0;
    while off < len {
        let n = libc::send(
            fd,
            buf.add(off) as *const c_void,
            len - off,
            libc::MSG_NOSIGNAL,
        );
        if n <= 0 {
            if n < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            return -1;
        }
        off += n as usize;
    }
    0
}

unsafe fn recv_all(fd: c_int, buf: *mut u8, len: usize) -> c_int {
    let mut off: usize = 0;
    while off < len {
        let n = libc::recv(fd, buf.add(off) as *mut c_void, len - off, 0);
        if n <= 0 {
            if n < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            return -1;
        }
        off += n as usize;
    }
    0
}

unsafe fn send_fds(sock: c_int, data: *const u8, data_len: usize, fds: &[c_int]) -> c_int {
    let iov = libc::iovec {
        iov_base: data as *mut c_void,
        iov_len: data_len,
    };
    let space = libc::CMSG_SPACE((fds.len() * std::mem::size_of::<c_int>()) as u32) as usize;
    let cmsg_buf = libc::malloc(space) as *mut u8;
    if cmsg_buf.is_null() {
        return -1;
    }
    ptr::write_bytes(cmsg_buf, 0, space);
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf as *mut c_void;
    msg.msg_controllen = space;
    let cmsg = libc::CMSG_FIRSTHDR(&msg);
    (*cmsg).cmsg_level = libc::SOL_SOCKET;
    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
    (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * std::mem::size_of::<c_int>()) as u32) as usize;
    ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg) as *mut c_int, fds.len());
    let n = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
    libc::free(cmsg_buf as *mut c_void);
    if n == data_len as isize {
        0
    } else {
        -1
    }
}

unsafe fn recv_fds(
    sock: c_int,
    data: *mut u8,
    data_len: usize,
    fds: *mut c_int,
    fd_count: c_int,
    fds_received: *mut c_int,
) -> c_int {
    let mut iov = libc::iovec {
        iov_base: data as *mut c_void,
        iov_len: data_len,
    };
    let space =
        libc::CMSG_SPACE((fd_count as usize * std::mem::size_of::<c_int>()) as u32) as usize;
    let cmsg_buf = libc::malloc(space) as *mut u8;
    if cmsg_buf.is_null() {
        return -1;
    }
    ptr::write_bytes(cmsg_buf, 0, space);
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf as *mut c_void;
    msg.msg_controllen = space;
    let n = libc::recvmsg(sock, &mut msg, 0);
    if n <= 0 {
        libc::free(cmsg_buf as *mut c_void);
        return -1;
    }
    *fds_received = 0;
    let cmsg = libc::CMSG_FIRSTHDR(&msg);
    if !cmsg.is_null()
        && (*cmsg).cmsg_level == libc::SOL_SOCKET
        && (*cmsg).cmsg_type == libc::SCM_RIGHTS
    {
        let mut count = ((*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<c_int>();
        if count > fd_count as usize {
            count = fd_count as usize;
        }
        ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg) as *const c_int, fds, count);
        *fds_received = count as c_int;
    }
    libc::free(cmsg_buf as *mut c_void);
    n as c_int
}

unsafe fn connect_unix(path: *const c_char) -> c_int {
    let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
    if fd < 0 {
        return -1;
    }
    let mut addr: libc::sockaddr_un = std::mem::zeroed();
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path_bytes = CStr::from_ptr(path).to_bytes();
    let cap = addr.sun_path.len() - 1;
    let copy_len = path_bytes.len().min(cap);
    ptr::copy_nonoverlapping(
        path_bytes.as_ptr(),
        addr.sun_path.as_mut_ptr() as *mut u8,
        copy_len,
    );
    // sun_path is zero-initialized, so it remains NUL-terminated.
    if libc::connect(
        fd,
        &addr as *const libc::sockaddr_un as *const libc::sockaddr,
        std::mem::size_of::<libc::sockaddr_un>() as u32,
    ) < 0
    {
        libc::close(fd);
        return -1;
    }
    fd
}

// ============================================================================
// display_producer (was display_producer.c)
// ============================================================================

unsafe fn release_consumer_resources(ctx: &mut display_ctx) {
    for i in 0..ctx.buf_count as usize {
        if ctx.dmabuf_fds[i] >= 0 {
            libc::close(ctx.dmabuf_fds[i]);
            ctx.dmabuf_fds[i] = -1;
        }
    }
    ctx.buf_count = 0;

    if ctx.data_fd >= 0 {
        libc::close(ctx.data_fd);
        ctx.data_fd = -1;
    }
    if ctx.buf_ready_efd >= 0 {
        libc::close(ctx.buf_ready_efd);
        ctx.buf_ready_efd = -1;
    }
    if ctx.fence_fd >= 0 {
        libc::close(ctx.fence_fd);
        ctx.fence_fd = -1;
    }
    if ctx.audio_fd >= 0 {
        libc::close(ctx.audio_fd);
        ctx.audio_fd = -1;
    }
    if ctx.pending_render_fence >= 0 {
        libc::close(ctx.pending_render_fence);
        ctx.pending_render_fence = -1;
    }
    if !ctx.shm_ptr.is_null() {
        libc::munmap(ctx.shm_ptr as *mut c_void, std::mem::size_of::<u32>());
        ctx.shm_ptr = ptr::null_mut();
    }
    if ctx.shm_fd >= 0 {
        libc::close(ctx.shm_fd);
        ctx.shm_fd = -1;
    }
}

unsafe fn enter_fallback(ctx: &mut display_ctx) {
    if ctx.fallback {
        return;
    }
    ctx.fallback = true;
    release_consumer_resources(ctx);
    if let Some(cb) = ctx.fallback_cb {
        cb(ctx.fallback_userdata);
    }
}

unsafe fn pickup_fds(ctx: &mut display_ctx) -> c_int {
    let hdr = ctrl_msg {
        type_: CTRL_MSG_PICKUP_FDS,
        size: 0,
    };
    if send_all(
        ctx.ctrl_fd,
        &hdr as *const ctrl_msg as *const u8,
        std::mem::size_of::<ctrl_msg>(),
    ) < 0
    {
        return -1;
    }

    let mut pfd = libc::pollfd {
        fd: ctx.ctrl_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if libc::poll(&mut pfd, 1, HANDSHAKE_TIMEOUT_MS) <= 0 {
        return -1;
    }

    let mut fds: [c_int; 5] = [-1; 5];
    let mut fd_count: c_int = 0;
    let mut resp = ctrl_msg::default();
    let n = recv_fds(
        ctx.ctrl_fd,
        &mut resp as *mut ctrl_msg as *mut u8,
        std::mem::size_of::<ctrl_msg>(),
        fds.as_mut_ptr(),
        5,
        &mut fd_count,
    );
    if n <= 0 || resp.type_ != CTRL_MSG_FDS_READY || fd_count < 5 {
        for i in 0..fd_count as usize {
            if fds[i] >= 0 {
                libc::close(fds[i]);
            }
        }
        return -1;
    }

    ctx.buf_ready_efd = fds[0];
    ctx.fence_fd = fds[1];
    ctx.data_fd = fds[2];
    ctx.shm_fd = fds[3];
    ctx.audio_fd = fds[4];

    let mapped = libc::mmap(
        ptr::null_mut(),
        std::mem::size_of::<u32>(),
        libc::PROT_READ,
        libc::MAP_SHARED,
        ctx.shm_fd,
        0,
    );
    if mapped == libc::MAP_FAILED {
        ctx.shm_ptr = ptr::null_mut();
        return -1;
    }
    ctx.shm_ptr = mapped as *mut u32;
    0
}

unsafe fn receive_dmabufs_inner(ctx: &mut display_ctx) -> c_int {
    let mut pfd = libc::pollfd {
        fd: ctx.data_fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    if libc::poll(&mut pfd, 1, HANDSHAKE_TIMEOUT_MS) <= 0 {
        return -1;
    }
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        return -1;
    }

    let mut dhdr = data_msg::default();
    let mut fds: [c_int; MAX_BUFS] = [-1; MAX_BUFS];
    let mut fd_count: c_int = 0;
    let n = recv_fds(
        ctx.data_fd,
        &mut dhdr as *mut data_msg as *mut u8,
        std::mem::size_of::<data_msg>(),
        fds.as_mut_ptr(),
        MAX_BUFS as c_int,
        &mut fd_count,
    );
    if n < std::mem::size_of::<data_msg>() as c_int || fd_count < 1 {
        for i in 0..fd_count as usize {
            if fds[i] >= 0 {
                libc::close(fds[i]);
            }
        }
        return -1;
    }

    if dhdr.type_ != DATA_MSG_BUFS_READY {
        for i in 0..fd_count as usize {
            if fds[i] >= 0 {
                libc::close(fds[i]);
            }
        }
        return -1;
    }

    let count = dhdr.size as usize / std::mem::size_of::<buf_info>();
    if count != fd_count as usize || count > MAX_BUFS {
        for i in 0..fd_count as usize {
            if fds[i] >= 0 {
                libc::close(fds[i]);
            }
        }
        return -1;
    }

    let mut infos: [buf_info; MAX_BUFS] = [buf_info::default(); MAX_BUFS];
    if recv_all(
        ctx.data_fd,
        &mut infos[0] as *mut buf_info as *mut u8,
        dhdr.size as usize,
    ) < 0
    {
        for i in 0..fd_count as usize {
            if fds[i] >= 0 {
                libc::close(fds[i]);
            }
        }
        return -1;
    }

    // Drop the previous set, then install the new one.
    for i in 0..ctx.buf_count as usize {
        if ctx.dmabuf_fds[i] >= 0 {
            libc::close(ctx.dmabuf_fds[i]);
            ctx.dmabuf_fds[i] = -1;
        }
    }
    for i in 0..count {
        ctx.dmabuf_fds[i] = fds[i];
        ctx.dmabuf_infos[i] = infos[i];
    }
    ctx.buf_count = count as c_int;
    0
}

unsafe fn receive_dmabufs(ctx: &mut display_ctx) -> c_int {
    if ctx.buf_count > 0 {
        return 0;
    }
    receive_dmabufs_inner(ctx)
}

pub unsafe fn connect_to_deamon(out: *mut *mut display_ctx, socket_path: *const c_char) -> c_int {
    let mut ctx = Box::new(display_ctx {
        ctrl_fd: -1,
        data_fd: -1,
        buf_ready_efd: -1,
        fence_fd: -1,
        shm_fd: -1,
        audio_fd: -1,
        pending_render_fence: -1,
        shm_ptr: ptr::null_mut(),
        screen_w: 0,
        screen_h: 0,
        pixel_format: 0,
        refresh: 0,
        fallback: true,
        dmabuf_fds: [-1; MAX_BUFS],
        dmabuf_infos: [buf_info::default(); MAX_BUFS],
        buf_count: 0,
        fallback_cb: None,
        fallback_userdata: ptr::null_mut(),
    });

    ctx.ctrl_fd = connect_unix(socket_path);
    if ctx.ctrl_fd < 0 {
        return -1;
    }

    let hdr = ctrl_msg {
        type_: CTRL_MSG_PRODUCER_HELLO,
        size: 0,
    };
    if send_all(
        ctx.ctrl_fd,
        &hdr as *const ctrl_msg as *const u8,
        std::mem::size_of::<ctrl_msg>(),
    ) < 0
    {
        libc::close(ctx.ctrl_fd);
        return -1;
    }

    // ctrl_msg + screen_info, laid out back-to-back (matches the C recv).
    let mut buf = [0u8; std::mem::size_of::<ctrl_msg>() + std::mem::size_of::<screen_info>()];
    if recv_all(ctx.ctrl_fd, buf.as_mut_ptr(), buf.len()) < 0 {
        libc::close(ctx.ctrl_fd);
        return -1;
    }

    let resp = *(buf.as_ptr() as *const ctrl_msg);
    if resp.type_ != CTRL_MSG_SCREEN_INFO
        || resp.size != std::mem::size_of::<screen_info>() as u32
    {
        libc::close(ctx.ctrl_fd);
        return -1;
    }

    let si = *(buf.as_ptr().add(std::mem::size_of::<ctrl_msg>()) as *const screen_info);
    ctx.screen_w = si.width;
    ctx.screen_h = si.height;
    ctx.pixel_format = si.format;
    ctx.refresh = si.refresh;

    *out = Box::into_raw(ctx);
    0
}

pub unsafe fn disconnect(ctx: *mut display_ctx) {
    if ctx.is_null() {
        return;
    }
    let mut ctx = Box::from_raw(ctx);
    release_consumer_resources(&mut ctx);
    if ctx.ctrl_fd >= 0 {
        libc::close(ctx.ctrl_fd);
    }
}

pub unsafe fn get_screen_info(
    ctx: *mut display_ctx,
    width: *mut u32,
    height: *mut u32,
    format: *mut u32,
    refresh: *mut u32,
) -> c_int {
    let ctx = &mut *ctx;
    *width = ctx.screen_w;
    *height = ctx.screen_h;
    *format = ctx.pixel_format;
    *refresh = ctx.refresh;
    0
}

pub unsafe fn set_render_fence(ctx: *mut display_ctx, fence_fd: c_int) {
    let ctx = &mut *ctx;
    if ctx.pending_render_fence >= 0 {
        libc::close(ctx.pending_render_fence);
    }
    ctx.pending_render_fence = fence_fd;
}

pub unsafe fn trigger_refresh(ctx: *mut display_ctx) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        if ctx.pending_render_fence >= 0 {
            libc::close(ctx.pending_render_fence);
            ctx.pending_render_fence = -1;
        }
        return 0;
    }

    // One render-done byte on the dedicated fence channel; the render fence
    // rides as SCM_RIGHTS ancillary data when present.
    let b: u8 = 0;
    let iov = libc::iovec {
        iov_base: &b as *const u8 as *mut c_void,
        iov_len: 1,
    };
    let space = libc::CMSG_SPACE(std::mem::size_of::<c_int>() as u32) as usize;
    let cmsg_buf = libc::malloc(space) as *mut u8;
    if !cmsg_buf.is_null() {
        ptr::write_bytes(cmsg_buf, 0, space);
    }
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
    msg.msg_iovlen = 1;
    if ctx.pending_render_fence >= 0 && !cmsg_buf.is_null() {
        msg.msg_control = cmsg_buf as *mut c_void;
        msg.msg_controllen = space;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<c_int>() as u32) as usize;
        ptr::copy_nonoverlapping(
            &ctx.pending_render_fence as *const c_int,
            libc::CMSG_DATA(cmsg) as *mut c_int,
            1,
        );
    }
    libc::sendmsg(
        ctx.fence_fd,
        &msg,
        libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
    );
    if !cmsg_buf.is_null() {
        libc::free(cmsg_buf as *mut c_void);
    }
    if ctx.pending_render_fence >= 0 {
        libc::close(ctx.pending_render_fence);
        ctx.pending_render_fence = -1;
    }
    0
}

pub unsafe fn poll_input_event(
    ctx: *mut display_ctx,
    event: *mut InputEvent,
    timeout_ms: c_int,
) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        return 0;
    }

    let mut pfd = libc::pollfd {
        fd: ctx.data_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if libc::poll(&mut pfd, 1, timeout_ms) <= 0 {
        return 0;
    }
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        enter_fallback(ctx);
        return -1;
    }

    let mut msg_buf = [0u8; std::mem::size_of::<data_msg>() + std::mem::size_of::<InputEvent>()];
    let n = libc::recv(
        ctx.data_fd,
        msg_buf.as_mut_ptr() as *mut c_void,
        msg_buf.len(),
        libc::MSG_PEEK,
    );
    if n < std::mem::size_of::<data_msg>() as isize {
        return 0;
    }

    let hdr = *(msg_buf.as_ptr() as *const data_msg);

    if hdr.type_ == DATA_MSG_BUFS_READY {
        if receive_dmabufs_inner(ctx) < 0 {
            enter_fallback(ctx);
            return -1;
        }
        return -2;
    }

    if hdr.type_ != DATA_MSG_INPUT_EVENT {
        return 0;
    }

    if recv_all(ctx.data_fd, msg_buf.as_mut_ptr(), msg_buf.len()) < 0 {
        return -1;
    }

    ptr::copy_nonoverlapping(
        msg_buf.as_ptr().add(std::mem::size_of::<data_msg>()) as *const u8,
        event as *mut u8,
        std::mem::size_of::<InputEvent>(),
    );
    1
}

pub unsafe fn poll_input_event_extend_data(
    ctx: *mut display_ctx,
    payload: *mut c_void,
    size: usize,
    timeout_ms: c_int,
) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        return 0;
    }
    let mut pfd = libc::pollfd {
        fd: ctx.data_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if libc::poll(&mut pfd, 1, timeout_ms) <= 0 {
        return 0;
    }
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        enter_fallback(ctx);
        return -1;
    }
    if recv_all(ctx.data_fd, payload as *mut u8, size) < 0 {
        return -1;
    }
    1
}

pub unsafe fn poll_input_event_extend_fds(
    ctx: *mut display_ctx,
    fds: *mut c_int,
    fd_count: c_int,
    timeout_ms: c_int,
) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        return 0;
    }
    let mut pfd = libc::pollfd {
        fd: ctx.data_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if libc::poll(&mut pfd, 1, timeout_ms) <= 0 {
        return 0;
    }
    if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        enter_fallback(ctx);
        return -1;
    }
    let mut hdr = data_msg::default();
    let mut received: c_int = 0;
    let n = recv_fds(
        ctx.data_fd,
        &mut hdr as *mut data_msg as *mut u8,
        std::mem::size_of::<data_msg>(),
        fds,
        fd_count,
        &mut received,
    );
    if n < std::mem::size_of::<data_msg>() as c_int || received < 1 {
        return -1;
    }
    if hdr.type_ != DATA_MSG_INPUT_EXTEND_FDS {
        for i in 0..received as usize {
            libc::close(*fds.add(i));
        }
        return -1;
    }
    received
}

unsafe fn push_output_event(ctx: *mut display_ctx, event: *const OutputEvent) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        return 0;
    }
    let hdr = data_msg {
        type_: DATA_MSG_OUTPUT_EVENT,
        size: std::mem::size_of::<OutputEvent>() as u32,
    };
    let mut msg = [0u8; std::mem::size_of::<data_msg>() + std::mem::size_of::<OutputEvent>()];
    ptr::copy_nonoverlapping(
        &hdr as *const data_msg as *const u8,
        msg.as_mut_ptr(),
        std::mem::size_of::<data_msg>(),
    );
    ptr::copy_nonoverlapping(
        event as *const OutputEvent as *const u8,
        msg.as_mut_ptr().add(std::mem::size_of::<data_msg>()),
        std::mem::size_of::<OutputEvent>(),
    );
    if send_all(ctx.data_fd, msg.as_ptr(), msg.len()) < 0 {
        enter_fallback(ctx);
        return -1;
    }
    0
}

pub unsafe fn push_output_event_with_length(
    ctx: *mut display_ctx,
    event: *const OutputEvent,
    payload: *const c_void,
    size: usize,
) -> c_int {
    let ctx = &mut *ctx;
    if ctx.fallback {
        return 0;
    }
    let hdr = data_msg {
        type_: DATA_MSG_OUTPUT_EVENT,
        size: std::mem::size_of::<OutputEvent>() as u32,
    };
    let total = std::mem::size_of::<data_msg>() + std::mem::size_of::<OutputEvent>() + size;
    let mut msg = vec![0u8; total];
    ptr::copy_nonoverlapping(
        &hdr as *const data_msg as *const u8,
        msg.as_mut_ptr(),
        std::mem::size_of::<data_msg>(),
    );
    ptr::copy_nonoverlapping(
        event as *const OutputEvent as *const u8,
        msg.as_mut_ptr().add(std::mem::size_of::<data_msg>()),
        std::mem::size_of::<OutputEvent>(),
    );
    if size > 0 {
        ptr::copy_nonoverlapping(
            payload as *const u8,
            msg.as_mut_ptr()
                .add(std::mem::size_of::<data_msg>() + std::mem::size_of::<OutputEvent>()),
            size,
        );
    }
    if send_all(ctx.data_fd, msg.as_ptr(), total) < 0 {
        enter_fallback(ctx);
        return -1;
    }
    0
}

pub unsafe fn push_resources_request(
    ctx: *mut display_ctx,
    service_type: u32,
    args: *const u32,
) -> c_int {
    let mut ev: OutputEvent = std::mem::zeroed();
    ev.type_ = OUTPUT_TYPE_RESOURCES_REQUEST;
    if !args.is_null() {
        let a = std::slice::from_raw_parts(args, 3);
        ev.data = OutputEventData {
            resources_request: ResourceRequestEvent {
                type_: service_type,
                args: [a[0], a[1], a[2]],
            },
        };
    } else {
        ev.data = OutputEventData {
            resources_request: ResourceRequestEvent {
                type_: service_type,
                args: [0; 3],
            },
        };
    }
    push_output_event(ctx, &ev)
}

pub unsafe fn set_fallback_callback(
    ctx: *mut display_ctx,
    on_fallback: Option<unsafe extern "C" fn(*mut c_void)>,
    userdata: *mut c_void,
) -> c_int {
    let ctx = &mut *ctx;
    ctx.fallback_cb = on_fallback;
    ctx.fallback_userdata = userdata;
    0
}

pub unsafe fn is_fallback(ctx: *mut display_ctx) -> bool {
    (*ctx).fallback
}

pub unsafe fn try_exit_fallback(ctx: *mut display_ctx) -> c_int {
    let ctx = &mut *ctx;
    if !ctx.fallback {
        return 0;
    }
    if pickup_fds(ctx) < 0 {
        release_consumer_resources(ctx);
        return -1;
    }
    if receive_dmabufs(ctx) < 0 {
        release_consumer_resources(ctx);
        return -1;
    }
    ctx.fallback = false;
    0
}

pub unsafe fn get_data_fd(ctx: *mut display_ctx) -> c_int {
    (*ctx).data_fd
}

pub unsafe fn get_audio_fd(ctx: *mut display_ctx) -> c_int {
    let ctx = &*ctx;
    if ctx.fallback {
        -1
    } else {
        ctx.audio_fd
    }
}

pub unsafe fn get_buffer_ready_fd(ctx: *mut display_ctx) -> c_int {
    (*ctx).buf_ready_efd
}

pub unsafe fn get_buf_count(ctx: *mut display_ctx) -> c_int {
    (*ctx).buf_count
}

pub unsafe fn get_selected_idx(ctx: *mut display_ctx) -> c_int {
    let ctx = &*ctx;
    if ctx.shm_ptr.is_null() {
        return 0;
    }
    let idx = std::ptr::read_volatile(ctx.shm_ptr);
    if idx < ctx.buf_count as u32 {
        idx as c_int
    } else {
        0
    }
}

pub unsafe fn get_dmabuf_fd_at(ctx: *mut display_ctx, idx: c_int) -> c_int {
    let ctx = &*ctx;
    if idx < 0 || idx >= ctx.buf_count {
        return -1;
    }
    ctx.dmabuf_fds[idx as usize]
}

pub unsafe fn get_dmabuf_info_at(ctx: *mut display_ctx, idx: c_int, info: *mut buf_info) -> c_int {
    let ctx = &*ctx;
    if idx < 0 || idx >= ctx.buf_count {
        return -1;
    }
    *info = ctx.dmabuf_infos[idx as usize];
    0
}

// Audio/camera bridges remain C (linked when have_anland_audio is set).
#[cfg(have_anland_audio)]
extern "C" {
    pub fn anland_audio_start() -> c_int;
    pub fn anland_audio_stop();
    pub fn anland_audio_set_fd(audio_fd: c_int);
    pub fn anland_camera_start() -> c_int;
    pub fn anland_camera_stop();
    pub fn anland_camera_set_resources(ctrl_fd: c_int, stream_fds: *const c_int, num_cameras: c_int);
    pub fn anland_camera_clear();
}
