#!/usr/bin/env python3
"""Exact-source trackA patch; run with python3 -I, never from the source tree."""
import argparse
import hashlib
from pathlib import Path

NIRI_COMMIT = "8ed0da44d974c32c6877d2f4630c314da0717ecb"
SMITHAY_COMMIT = "ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef"
BLOBS = {
    "Cargo.toml": "fa7164e97ee5e729ce2a9228a666c94175cc3b3a",
    "Cargo.lock": "b03ba15c065df431cef5d4cde6e9810732ce6c5c",
    "src/niri.rs": "ec40e01c140b02847fb3337df29d98b7dcc68014",
    "src/backend/winit.rs": "a9bcc5155145b622c0494c2c6274e2f93b2983dc",
}


def replace(text, old, new, count=1):
    if text.count(old) != count:
        raise SystemExit(f"anchor mismatch: expected {count}: {old[:100]!r}")
    return text.replace(old, new)


def sources(root):
    result = {}
    for name, expected in BLOBS.items():
        data = (root / name).read_bytes()
        actual = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if actual != expected:
            raise SystemExit(f"unverified source: {name}: {actual} != {expected}")
        result[name] = data.decode()
    return result


# No tracing filter dependency. Hard cap applies to all niri diagnostic sites together.
HELPER = r'''
// trackA: observational only; enable explicitly, bounded to <=10000 records/process.
pub(crate) fn frame_diag_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("NIRI_FRAME_DIAG").as_deref() == Ok("1"))
}

pub(crate) fn frame_diag(args: std::fmt::Arguments<'_>) {
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
        let monotonic_us = get_monotonic_time().as_micros();
        if n == limit {
            let _ = writeln!(std::io::stderr().lock(), "FRAME_DIAG niri monotonic_us={monotonic_us} event=cap_exhausted limit={limit}");
        } else {
            let _ = writeln!(std::io::stderr().lock(), "FRAME_DIAG niri monotonic_us={monotonic_us} t_us={elapsed} {args}");
        }
    }
}
'''


def diagnostic_niri(text):
    old = '''            if current_primary_output.as_ref() != Some(output) {
                return None;
            }'''
    new = '''            if current_primary_output.as_ref() != Some(output) {
                if frame_diag_enabled() {
                    frame_diag(format_args!("event=normal-select surface={:?} output={:?} primary={:?} sequence={} reason=primary-mismatch", surface.id(), output.name(), current_primary_output.as_ref().map(|o| o.name()), sequence));
                }
                return None;
            }'''
    text = replace(text, old, new)
    old = '''            if send {
                *last_sent_at = Some((output.clone(), sequence));'''
    new = '''            if frame_diag_enabled() {
                frame_diag(format_args!("event=normal-select surface={:?} output={:?} primary={:?} sequence={} previous={:?} reason={}", surface.id(), output.name(), current_primary_output.as_ref().map(|o| o.name()), sequence, last_sent_at.as_ref().map(|(o, s)| (o.name(), *s)), if send { "selected" } else { "duplicate-sequence" }));
            }
            if send {
                *last_sent_at = Some((output.clone(), sequence));'''
    text = replace(text, old, new)
    old = '        let _span = tracy_client::span!("Niri::send_frame_callbacks_on_fallback_timer");'
    text = replace(text, old, old + '''
        frame_diag(format_args!("event=fallback-timer throttle_ms={:?}", FRAME_CALLBACK_THROTTLE.map(|t| t.as_millis())));''')
    # Log each fallback selection, without changing its None return value.
    start = text.index('    pub fn send_frame_callbacks_on_fallback_timer(')
    end = text.index('\n    pub fn ', start + 10)
    block = text[start:end]
    block = replace(block, '|_, _| None,', '''|surface, states| {
                    if frame_diag_enabled() {
                        frame_diag(format_args!("event=fallback-select surface={:?} primary={:?} reason=timer-none", surface.id(), surface_primary_scanout_output(surface, states).as_ref().map(|o| o.name())));
                    }
                    None
                },''', 5)
    text = text[:start] + block + text[end:]
    # Two attribution paths: regular windows (including offscreens) and layer/xray.
    old = '''                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,'''
    new = '''                if frame_diag_enabled() {
                    frame_diag(format_args!("event=attribution surface={:?} output={:?} element={:?} presented={} previous_primary={:?}", surface.id(), output.name(), id, render_element_states.element_was_presented(id.clone()), primary_scanout_output.current_output().as_ref().map(|o| o.name())));
                }
                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,'''
    text = replace(text, old, new, 2)
    # Inspect result using existing lock; do not relock the surface data map.
    for tail in ('                    render_element_states,\n                    |_, _, output, _| output,\n                );',
                 '                    // Layer surfaces are shown only on one output at a time.\n                    |_, _, output, _| output,\n                );'):
        text = replace(text, tail, tail + '''
                if frame_diag_enabled() {
                    frame_diag(format_args!("event=attribution-result surface={:?} output={:?} primary={:?}", surface.id(), output.name(), primary_scanout_output.current_output().as_ref().map(|o| o.name())));
                }''')
    return text + HELPER


def diagnostic_winit(text):
    anchor = '        let _span = tracy_client::span!("Winit::render");'
    text = replace(text, anchor, anchor + '''
        let diag_start = crate::niri::frame_diag_enabled().then(std::time::Instant::now);
        let mut diag_submit_us = None;''')
    anchor = '        niri.update_primary_scanout_output(output, &res.states);'
    text = replace(text, anchor, '''        let diag_render_us = diag_start.map(|t| t.elapsed().as_micros());
        let diag_had_damage = res.damage.is_some();
''' + anchor)
    anchor = '            self.backend.submit(Some(damage)).unwrap();'
    text = replace(text, anchor, '''            let diag_submit_start = diag_start.map(|_| std::time::Instant::now());
''' + anchor + '''
            diag_submit_us = diag_submit_start.map(|t| t.elapsed().as_micros());''')
    anchor = '''        // FIXME: this should wait until a frame callback from the host compositor, but it redraws'''
    text = replace(text, anchor, '''        if let Some(start) = diag_start {
            crate::niri::frame_diag(format_args!("event=winit-render output={:?} sequence={} damage={} render_us={:?} submit_us={:?} total_us={} animations={}", output.name(), output_state.frame_callback_sequence, diag_had_damage, diag_render_us, diag_submit_us, start.elapsed().as_micros(), output_state.unfinished_animations_remain));
        }

''' + anchor)
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--mode", choices=("baseline", "diagnostic"), required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    text = sources(root)  # Validate all inputs before any writes; reapplication fails closed.
    manifest = text["Cargo.toml"]
    dependency = f'git = "https://github.com/Smithay/smithay.git"\nrev = "{SMITHAY_COMMIT}"'
    manifest = replace(manifest, dependency, 'path = "../smithay"', 2)
    # The second package lives in the Smithay subdirectory, not its root.
    manifest = replace(manifest, '[workspace.dependencies.smithay-drm-extras]\n# version = "0.1.0"\npath = "../smithay"',
                       '[workspace.dependencies.smithay-drm-extras]\n# version = "0.1.0"\npath = "../smithay/smithay-drm-extras"')
    text["Cargo.toml"] = manifest
    source = f'source = "git+https://github.com/Smithay/smithay.git?rev={SMITHAY_COMMIT}#{SMITHAY_COMMIT}"\n'
    text["Cargo.lock"] = replace(text["Cargo.lock"], source, "", 2)
    if args.mode == "diagnostic":
        text["src/niri.rs"] = diagnostic_niri(text["src/niri.rs"])
        text["src/backend/winit.rs"] = diagnostic_winit(text["src/backend/winit.rs"])
    for name, value in text.items():
        (root / name).write_bytes(value.encode())
    print(f"NIRI_FRAME_PATCH_COMPLETE mode={args.mode} niri={NIRI_COMMIT} smithay={SMITHAY_COMMIT}")


if __name__ == "__main__":
    main()
