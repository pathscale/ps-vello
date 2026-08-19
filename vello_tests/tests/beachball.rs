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

use std::time::{Duration, Instant};

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

/// The budget one frame has at 120Hz, which is the refresh rate this was
/// reported on. A renderer that blocks past this cannot keep up even when it
/// has nothing else to do.
const REFRESH_INTERVAL: Duration = Duration::from_micros(8333);

#[test]
#[cfg_attr(skip_gpu_tests, ignore)]
fn successive_frames_do_not_block_past_a_refresh_interval() {
    // Enough frames that the two-frames-ago submission is live for most of the
    // run: the wait cannot happen at all until the third frame, so a shorter
    // sequence would pass whether or not it blocks.
    const FRAMES: usize = 12;
    let scenes: Vec<Scene> = (0..FRAMES).map(frame).collect();

    let params = TestParams::new("beachball_no_block", 150, 150);

    let start = Instant::now();
    let image = pollster::block_on(vello_tests::get_scene_images_sequence(&params, &scenes))
        .expect("the sequence must render");
    let elapsed = start.elapsed();

    // The output still has to be right. A renderer that skips synchronisation
    // to go fast fails here rather than passing on the timing alone: the
    // "invalid empty image" attempt drew nothing, and a starved reallocation
    // check drops content once the bump buffers are too small.
    let mut red = 0_usize;
    for pixel in image.data.data().chunks_exact(4) {
        let &[r, g, b, a] = pixel else { unreachable!() };
        if r == 255 && g == 0 && b == 0 && a == 255 {
            red += 1;
        }
    }
    assert!(
        red > 0,
        "the last frame must still draw its square; a renderer that skips \
         synchronisation renders nothing and would otherwise pass on time alone"
    );

    // Device setup dominates a short run and is not what this measures, so the
    // budget is generous: the failure it exists to catch parked the main thread
    // for 99% of its samples, which is orders of magnitude past this.
    let budget = REFRESH_INTERVAL * (FRAMES as u32) * 8;
    assert!(
        elapsed < budget,
        "rendering {FRAMES} frames took {elapsed:?}, over the {budget:?} budget: \
         the renderer is blocking the calling thread between frames, which is a \
         beachball for an embedder that renders on the UI thread"
    );
}
