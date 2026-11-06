#!/usr/bin/env python3
"""Instrument actual Smithay callback drains; exact blob, bounded opt-in logs."""
import argparse
import hashlib
from pathlib import Path

COMMIT = "ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef"
BLOB = "e92a81ecac5e2c872e8eef1faef0c2e04d51e45b"
HELPER = r'''
// trackA: independent bounded budget, shared runtime opt-in with niri.
fn frame_diag_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("NIRI_FRAME_DIAG").as_deref() == Ok("1"))
}

fn frame_diag(args: std::fmt::Arguments<'_>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    if !frame_diag_enabled() {
        return;
    }
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("NIRI_FRAME_DIAG_LIMIT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(2000).min(10000)
    });
    if let Ok(n) = COUNT.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
        |n| (n <= limit).then_some(n + 1)) {
        use std::io::Write as _;
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed().as_micros();
        let monotonic_us = crate::utils::Clock::<crate::utils::Monotonic>::new().now().as_micros();
        if n == limit {
            let _ = writeln!(std::io::stderr().lock(), "FRAME_DIAG smithay monotonic_us={monotonic_us} event=cap_exhausted limit={limit}");
        } else {
            let _ = writeln!(std::io::stderr().lock(), "FRAME_DIAG smithay monotonic_us={monotonic_us} t_us={elapsed} {args}");
        }
    }
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    path = args.root.resolve() / "src/desktop/wayland/utils.rs"
    data = path.read_bytes()
    actual = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
    if actual != BLOB:
        raise SystemExit(f"unverified Smithay source: {actual} != {BLOB}")
    text = data.decode()
    anchor = '''            if send_frame_callback {
                // the surface may not have any user_data if it is a subsurface and has not'''
    replacement = '''            // Only observe pending callbacks; preserve the original drain and timer update.
            let diag_pending = if frame_diag_enabled() {
                Some(states.cached_state.get::<SurfaceAttributes>().current().frame_callbacks.len())
            } else {
                None
            };
            let mut diag_drained = 0usize;
            if send_frame_callback {
                // the surface may not have any user_data if it is a subsurface and has not'''
    if text.count(anchor) != 1:
        raise SystemExit("Smithay send anchor mismatch")
    text = text.replace(anchor, replacement)
    anchor = '''                    callback.done(time.as_millis() as u32);
                }
            }
        },'''
    replacement = '''                    callback.done(time.as_millis() as u32);
                    if diag_pending.is_some() {
                        diag_drained += 1;
                    }
                }
            }
            if let Some(pending) = diag_pending {
                if pending > 0 {
                    frame_diag(format_args!("event=callback-drain surface={:?} output={:?} callback_time_ms={} on_primary={} overdue={} throttle_ms={:?} pending={} drained={} send={}", wayland_server::Resource::id(surface), output.name(), time.as_millis(), on_primary_scanout_output, frame_overdue, throttle.map(|t| t.as_millis()), pending, diag_drained, send_frame_callback));
                }
            }
        },'''
    if text.count(anchor) != 1:
        raise SystemExit("Smithay drain anchor mismatch")
    text = text.replace(anchor, replacement)
    path.write_bytes((text + HELPER).encode())
    print(f"NIRI_FRAME_SMITHAY_PATCH_COMPLETE smithay={COMMIT}")


if __name__ == "__main__":
    main()
