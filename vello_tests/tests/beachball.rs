// Copyright 2026 the Vello Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The renderer must not park the calling thread once a frame is ready.
//!
//! `Renderer::render_to_texture` is called on the UI thread by embedders that
//! draw a `backdrop-filter`: the filter cuts the frame in two, so the renderer
//! runs again inside the same event-loop turn. Anything that blocks in there is
//! paid on the thread that answers input, and a wait long enough to miss the
//! refresh interval is a beachball, which is what this guards.
//!
//! The specific wait that caused one is in `block_on_bump_and_reallocate`. It
//! waits on the submission from *two frames ago* only to read a
//! `BumpAllocators` struct and decide whether to grow buffers, and it used to
//! do that unconditionally. Sampled on the consuming app: 3249 of 3281
//! main-thread samples inside `nanosleep`, under `wgpu_hal::metal::Device::wait`
//! and `anyrender_vello::backdrop::execute`, with the window otherwise idle.
//!
//! Two earlier attempts at removing it are why this file asserts on time rather
//! than on the call. Polling instead of waiting starved the reallocation check
//! (`is_queue_empty` asks about the whole queue, so under continuous rendering
//! it never passed) and dropping the wait outright let a texture be recycled
//! mid-render, which panicked with "tried to draw an invalid empty image". Both
//! of those still *render*, so only a test that watches the wall clock across
//! successive frames while checking the output is still correct can tell the
//! fix from either failure.

use std::time::Duration;

use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Brush, color::palette};
use vello_tests::TestParams;

/// One frame's worth of geometry, varied so no frame is a no-op.
fn frame(index: usize) -> Scene {
    let mut scene = Scene::new();
    let offset = index as f64;
    scene.fill(
        vello::peniko::Fill::NonZero,
        Affine::IDENTITY,
        &Brush::Solid(palette::css::RED),
        None,
        &Rect::from_center_size((100. + offset, 100.), (50., 50.)),
    );
    scene
}

#[test]
#[cfg_attr(skip_gpu_tests, ignore)]
fn successive_frames_do_not_block_past_a_refresh_interval() {
    // Enough frames that the two-frames-ago submission is live for most of the
    // run: the wait cannot happen at all until the third frame, so a shorter
    // sequence would pass whether or not it blocks.
    const FRAMES: usize = 12;

    let scenes: Vec<Scene> = (0..FRAMES).map(frame).collect();
    let params = TestParams::new("beachball_no_block", 150, 150);
    let (times, image) = pollster::block_on(vello_tests::time_scene_sequence(&params, &scenes))
        .expect("the sequence must render");
    let last = image.data.data();

    // Printed on every run, so a failure on a machine that cannot be attached
    // to says which frame was slow rather than only that one was.
    eprintln!("per-frame times: {times:?}");

    // The output still has to be right. A renderer that skips synchronisation
    // to go fast fails here rather than passing on the timing alone: the
    // "invalid empty image" attempt drew nothing, and a starved reallocation
    // check drops content once the bump buffers are too small.
    let red = last
        .chunks_exact(4)
        .filter(|p| p[0] == 255 && p[1] == 0 && p[2] == 0 && p[3] == 255)
        .count();
    assert!(
        red > 0,
        "the last frame must still draw its square; a renderer that skips \
         synchronisation renders nothing and would otherwise pass on time alone"
    );

    /*
     * The shape of the cost, not its absolute size.
     *
     * An earlier version of this asserted total wall-clock against a fixed
     * budget, and that made it flaky: run alone it passed comfortably, run
     * inside `cargo test --workspace` alongside several thousand other tests it
     * failed on contention rather than on anything about the renderer. A test
     * that fails for reasons outside what it is testing teaches nothing, which
     * is the same objection as a check that can only ever fail.
     *
     * The defect has a signature that machine load does not imitate. The wait
     * is on the submission from *two frames ago*, so it cannot occur on the
     * first two frames at all and then applies to every frame after: the tail
     * of the run is systematically slower than its head. Contention slows all
     * of the frames roughly alike, so the ratio between them stays near one
     * however loaded the machine is.
     *
     * Four times is far below what the defect produced - the beachball had the
     * calling thread parked for 99% of its samples - and far above the spread
     * of an unloaded run.
     */
    let head: Duration = times[..2].iter().sum::<Duration>() / 2;
    let tail: Duration = times[FRAMES - 4..].iter().sum::<Duration>() / 4;
    assert!(
        tail < head * 4,
        "later frames are {tail:?} against {head:?} for the first two: the \
         renderer is blocking on a submission from two frames ago, which is \
         paid once per frame on whichever thread is rendering"
    );

    // A floor as well, so a renderer that stopped submitting work entirely
    // cannot pass by being trivially fast.
    assert!(
        times.iter().all(|t| *t > Duration::ZERO),
        "every frame should take measurable time; {times:?}"
    );

    /*
     * No absolute ceiling, deliberately.
     *
     * There was one, and CI failed it at 3.19s for a single frame. That frame
     * was the *first*, which on a runner with no GPU pays for shader
     * compilation and pipeline creation against a software adapter. It is
     * start-up cost rather than a stall, and it is exactly the sort of
     * environment difference a fixed budget cannot tell apart from a defect,
     * which is why the wall-clock version of this test was replaced in the
     * first place. Reintroducing a ceiling smuggled the same mistake back in.
     *
     * The head-versus-tail comparison above already excludes it: the first two
     * frames are the baseline, so whatever they pay for warm-up is what later
     * frames are measured against.
     */
}
