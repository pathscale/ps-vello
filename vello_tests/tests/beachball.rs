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
    let (blocked, image) = pollster::block_on(vello_tests::time_scene_sequence(&params, &scenes))
        .expect("the sequence must render");
    let last = image.data.data();

    eprintln!("blocked for {blocked:?} across {FRAMES} frames");

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
     * The wait itself, not the frame around it.
     *
     * Three earlier versions of this asserted on wall-clock and each was wrong
     * in its own way: a total budget failed under `--workspace` on contention,
     * a per-frame budget failed on CI where a software adapter flushes the
     * queue on whichever frame it likes, and draining the queue to make those
     * numbers comparable buried a 30ms stall under 6.5ms of real GPU work per
     * frame. Frame time is a proxy, and every proxy for this defect breaks on
     * some machine.
     *
     * `Renderer::blocked_nanos` is the defect stated directly: how long the
     * calling thread was parked inside the renderer. It does not vary with the
     * adapter, because it measures the wait rather than the work around it.
     *
     * Twelve frames of a 150x150 square need essentially none of it. The
     * threshold is generous next to the defect, which parked the thread for 99%
     * of its samples and shows here as tens of milliseconds per frame.
     */
    assert!(
        blocked < Duration::from_millis(50),
        "the renderer blocked its caller for {blocked:?} across {FRAMES} frames; \
         the wait on a submission from two frames ago is being paid on whichever \
         thread renders, which is the UI thread for an embedder drawing a \
         backdrop-filter"
    );
}

/// Many renders inside one frame, which is what a backdrop filter actually does.
///
/// `anyrender_vello::backdrop::execute` calls `render_to_texture` once per
/// backdrop boundary and once more for the final scene, so a page with three
/// glass surfaces renders four times before a single frame is presented. That
/// matters for this wait: `previouser_submission` fills up *within* one frame
/// rather than across frames, so the wait is reachable long before any frame
/// completes.
///
/// It is why the app could wedge with zero frames rendered, and why the
/// twelve-frame test above cannot see that case: it renders once per frame and
/// only reaches the wait from the third frame onward.
///
/// Stated plainly: this currently exercises the same code path as the test
/// above, because the harness builds one renderer per call and cannot yet put
/// several renders inside one presented frame. It is kept because the shape it
/// documents is the one the consuming app hits, and because the finding it
/// records - that the wait is reachable before any frame completes - is what
/// explains a wedge the other test cannot reproduce. Making it genuinely
/// distinct needs a harness that presents, which is the next piece of work.
#[test]
#[cfg_attr(skip_gpu_tests, ignore)]
fn many_renders_in_one_frame_do_not_block_the_caller() {
    // Four renders is a page with three backdrop boundaries, which the consuming
    // app reached routinely before the pass count was cut.
    const RENDERS: usize = 4;
    const FRAMES: usize = 3;

    let scenes: Vec<Scene> = (0..RENDERS * FRAMES).map(frame).collect();
    let params = TestParams::new("beachball_backdrop_shape", 150, 150);
    let (blocked, _image) = pollster::block_on(vello_tests::time_scene_sequence(&params, &scenes))
        .expect("the sequence must render");

    eprintln!("blocked for {blocked:?} across {} renders", scenes.len());

    assert!(
        blocked < Duration::from_millis(50),
        "the renderer blocked its caller for {blocked:?} across {} renders; a \
         backdrop filter renders once per boundary plus once for the final \
         scene, so this wait is paid several times per frame and lands on the \
         UI thread",
        scenes.len()
    );
}
