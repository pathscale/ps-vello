// Copyright 2022 the Vello Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use guillotiere::{AllocId, AtlasAllocator, size2};
use peniko::ImageData;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

const DEFAULT_ATLAS_SIZE: i32 = 1024;
const MAX_ATLAS_SIZE: i32 = 8192;
const EVICT_AFTER_GENERATIONS: u64 = 2;

#[derive(Default)]
pub struct Images<'a> {
    /// Width of the square image atlas texture.
    pub width: u32,
    /// Height of the square image atlas texture.
    pub height: u32,
    /// Number of resident images evicted during the current resolve pass.
    ///
    /// This is only used for renderer-side debug logging.
    pub evicted: usize,
    /// Images that must be uploaded in the current resolve pass, with atlas locations.
    pub images: &'a [(ImageData, u32, u32)],
}

#[derive(Clone)]
struct ResidentImage {
    image: ImageData,
    alloc_id: AllocId,
    x: u32,
    y: u32,
    dirty: bool,
    last_used_generation: u64,
}

pub(crate) struct ImageCache {
    atlas: AtlasAllocator,
    /// Side length the atlas starts at, and the floor [`Self::shrink_to_fit`]
    /// will not go below.
    initial_size: i32,
    /// Maximum side length for the square image atlas texture.
    max_size: i32,
    /// Monotonic counter for resolve passes, used to track when resident images were last used.
    generation: u64,
    /// Number of resident images evicted during the current resolve pass.
    ///
    /// This is exposed through [`Images::evicted`] for renderer-side debug logging, and also
    /// prevents repeated stale-eviction scans during the same resolve pass.
    evicted_in_resolve: usize,
    /// Map from image blob id to atlas residency.
    map: HashMap<u64, ResidentImage>,
    /// Images that must be uploaded in the current resolve pass, with atlas locations.
    images: Vec<(ImageData, u32, u32)>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    pub(crate) fn new() -> Self {
        Self::new_with_sizes(DEFAULT_ATLAS_SIZE, MAX_ATLAS_SIZE)
    }

    fn new_with_sizes(initial_size: i32, max_size: i32) -> Self {
        Self {
            atlas: AtlasAllocator::new(size2(initial_size, initial_size)),
            initial_size,
            max_size,
            generation: 0,
            evicted_in_resolve: 0,
            map: HashMap::default(),
            images: Vec::default(),
        }
    }

    pub(crate) fn begin_resolve(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.evicted_in_resolve = 0;
        self.images.clear();
    }

    pub(crate) fn restart_resolve_pass(&mut self) {
        self.images.clear();
        let previous_generation = self.generation.wrapping_sub(1);
        for resident in self.map.values_mut() {
            if resident.last_used_generation == self.generation {
                resident.last_used_generation = previous_generation;
            }
        }
    }

    pub(crate) fn images(&self) -> Images<'_> {
        Images {
            width: self.atlas.size().width as u32,
            height: self.atlas.size().height as u32,
            evicted: self.evicted_in_resolve,
            images: &self.images,
        }
    }

    /// Grow the atlas, doubling the shorter side each step.
    ///
    /// # Why not square
    ///
    /// The atlas used to be square and doubled both sides at once, which wastes
    /// half the growth whenever the content is wider than it is tall. Full-frame
    /// `backdrop-filter` snapshots are exactly that: two 2688x1800 rects need
    /// 5376x1800, which no 4096x4096 square can hold, so the atlas jumped to
    /// 8192x8192 and 256 MB to store 19 MB of pixels twice.
    ///
    /// Doubling the shorter side reaches 8192x4096 instead, which fits the same
    /// content in half the memory, and nothing downstream needs a square: the
    /// renderer already sizes its atlas texture from `images.width` and
    /// `images.height` independently.
    pub(crate) fn bump_size(&mut self) -> bool {
        let (mut width, mut height) = {
            let size = self.atlas.size();
            (size.width, size.height)
        };
        while width <= self.max_size && height <= self.max_size {
            if width <= height {
                width *= 2;
            } else {
                height *= 2;
            }
            if width > self.max_size || height > self.max_size {
                return false;
            }
            if self.repack_to(width, height) {
                self.images.clear();
                return true;
            }
        }
        false
    }

    /// Give the atlas back once the images that forced it to grow are gone.
    ///
    /// # Why this exists
    ///
    /// `bump_size` doubles a square `Rgba8` atlas and nothing ever halved it,
    /// so the atlas ratcheted to the high water mark of the session and stayed
    /// there for the life of the process. The ceiling is 8192, which is
    /// 8192 * 8192 * 4 = 256 MB.
    ///
    /// That ceiling is reachable in ordinary use. A `backdrop-filter` pass
    /// registers its full-frame snapshot as an image, and two full-viewport
    /// rects (2688x1800 on this display) cannot be packed into 4096x4096, so a
    /// frame with two backdrop boundaries escalates 1024 -> 2048 -> 4096 ->
    /// 8192 and the process then holds a quarter gigabyte of atlas forever.
    /// Measured on `AgencyZero`, that single 258 MB allocation was the largest
    /// item in a 1 GB graphics footprint, and it was present in every build.
    ///
    /// Shrinking is the same repack as growing, so it costs a repack of the
    /// live residents and nothing else. It only fires when the result is
    /// strictly smaller and everything still fits, so a steady scene never
    /// churns: the atlas settles at the size its own contents need.
    /// # Why moving residents is safe here, and only here
    ///
    /// A repack moves every resident image. That is safe only because this runs
    /// at the very start of `resolve_pending_images`, before any
    /// `pending_image.xy` has been recorded, so every position this frame uses
    /// is read after the move. Calling it any later leaves the recorded
    /// coordinates pointing at where the images used to be, every draw samples
    /// the wrong part of the atlas, and the result is a grey window: no panic,
    /// full frame rate, wrong pixels.
    ///
    /// `repack_to_size` marks everything it moves dirty, so an image that is
    /// drawn again in a later frame is re-uploaded to its new position before
    /// it is sampled.
    pub(crate) fn shrink_to_fit(&mut self) {
        let size = self.atlas.size();
        let (mut width, mut height) = (size.width, size.height);
        if width <= self.initial_size && height <= self.initial_size {
            return;
        }
        // Halve the longer side while everything still fits, mirroring the way
        // `bump_size` grows. `would_fit` is a dry run, so a failed probe leaves
        // the live atlas untouched.
        let mut best = None;
        loop {
            let (next_width, next_height) = if width >= height {
                (width / 2, height)
            } else {
                (width, height / 2)
            };
            if next_width < self.initial_size || next_height < self.initial_size {
                break;
            }
            if !self.would_fit(next_width, next_height) {
                break;
            }
            best = Some((next_width, next_height));
            width = next_width;
            height = next_height;
        }
        let Some((target_width, target_height)) = best else {
            return;
        };
        if self.repack_to(target_width, target_height) {
            self.images.clear();
        }
    }

    /// Whether every resident image would pack into a `width` by `height` atlas.
    ///
    /// A dry run: it must not disturb the live atlas, because the caller keeps
    /// using that atlas when the answer is no.
    fn would_fit(&self, width: i32, height: i32) -> bool {
        let mut atlas = AtlasAllocator::new(size2(width, height));
        self.map.values().all(|resident| {
            atlas
                .allocate(size2(resident.image.width as _, resident.image.height as _))
                .is_some()
        })
    }

    pub(crate) fn get_or_insert(&mut self, image: &ImageData) -> Option<(u32, u32)> {
        match self.map.entry(image.data.id()) {
            Entry::Occupied(mut occupied) => {
                let resident = occupied.get_mut();
                let xy = (resident.x, resident.y);
                if resident.last_used_generation != self.generation {
                    resident.last_used_generation = self.generation;
                    if resident.dirty {
                        self.images
                            .push((resident.image.clone(), resident.x, resident.y));
                    }
                }
                Some(xy)
            }
            Entry::Vacant(vacant) => {
                let alloc = self
                    .atlas
                    .allocate(size2(image.width as _, image.height as _))?;
                let x = alloc.rectangle.min.x as u32;
                let y = alloc.rectangle.min.y as u32;
                let resident = ResidentImage {
                    image: image.clone(),
                    alloc_id: alloc.id,
                    x,
                    y,
                    dirty: true,
                    last_used_generation: self.generation,
                };
                self.images.push((image.clone(), x, y));
                vacant.insert(resident);
                Some((x, y))
            }
        }
    }

    pub(crate) fn finish_resolve(&mut self) {
        for resident in self.map.values_mut() {
            if resident.last_used_generation == self.generation {
                resident.dirty = false;
            }
        }
    }

    pub(crate) fn mark_dirty(&mut self, image: &ImageData) {
        if let Some(resident) = self.map.get_mut(&image.data.id()) {
            resident.dirty = true;
        }
    }

    pub(crate) fn can_fit_image(&self, image: &ImageData) -> bool {
        image.width <= self.atlas.size().width as u32
            && image.height <= self.atlas.size().height as u32
    }

    pub(crate) fn evict_stale_entries(&mut self) -> bool {
        if self.evicted_in_resolve != 0 {
            return false;
        }
        let Some(stale_before) = self.generation.checked_sub(EVICT_AFTER_GENERATIONS) else {
            return false;
        };
        for (_id, resident) in self
            .map
            .extract_if(|_, resident| resident.last_used_generation < stale_before)
        {
            self.atlas.deallocate(resident.alloc_id);
            self.evicted_in_resolve += 1;
        }
        self.evicted_in_resolve != 0
    }

    /// Square repack, used by the tests that exercise atlas growth.
    ///
    /// `cfg(test)` rather than deleted: clippy sees no caller in the library
    /// build and the tests do not compile without it.
    #[cfg(test)]
    fn repack_to_size(&mut self, size: i32) -> bool {
        self.repack_to(size, size)
    }

    fn repack_to(&mut self, width: i32, height: i32) -> bool {
        let mut atlas = AtlasAllocator::new(size2(width, height));
        let mut entries: Vec<_> = self.map.iter().collect();
        entries.sort_by_key(|(id, _)| *id);
        let mut map = HashMap::with_capacity(self.map.len());
        for (id, resident) in entries {
            let Some(alloc) =
                atlas.allocate(size2(resident.image.width as _, resident.image.height as _))
            else {
                return false;
            };
            let mut resident = resident.clone();
            resident.alloc_id = alloc.id;
            resident.x = alloc.rectangle.min.x as u32;
            resident.y = alloc.rectangle.min.y as u32;
            resident.dirty = true;
            map.insert(*id, resident);
        }
        self.atlas = atlas;
        self.map = map;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peniko::{Blob, ImageAlphaType, ImageFormat};
    use std::sync::Arc;

    fn image(id_byte: u8, width: u32, height: u32) -> ImageData {
        let len = (width * height * 4) as usize;
        ImageData {
            data: Blob::new(Arc::new(vec![id_byte; len])),
            format: ImageFormat::Rgba8,
            width,
            height,
            alpha_type: ImageAlphaType::Alpha,
        }
    }

    #[test]
    fn atlas_size_persists_after_growth() {
        let mut cache = ImageCache::new_with_sizes(16, 64);
        assert_eq!(cache.atlas.size().width, 16);
        assert!(cache.bump_size());
        assert_eq!(cache.atlas.size().width, 32);
        cache.begin_resolve();
        assert_eq!(cache.atlas.size().width, 32);
    }

    /// The atlas gives its memory back once every image is gone.
    ///
    /// Without this the atlas ratchets to the session's high water mark and
    /// holds it forever: on a `backdrop-filter` pass that is a 256 MB texture
    /// kept for the life of the process.
    #[test]
    fn atlas_returns_to_the_floor_once_the_cache_drains() {
        let mut cache = ImageCache::new_with_sizes(16, 256);
        let big = image(1, 96, 96);

        cache.begin_resolve();
        while cache.get_or_insert(&big).is_none() {
            assert!(cache.bump_size(), "the atlas should reach a size that fits");
        }
        let grown = cache.atlas.size().width;
        assert!(grown >= 96, "a 96px image needs at least a 96px atlas");
        cache.finish_resolve();

        // Frames that draw nothing, which is what lets the big image go stale.
        for _ in 0..=EVICT_AFTER_GENERATIONS {
            cache.begin_resolve();
            cache.evict_stale_entries();
            cache.finish_resolve();
        }
        cache.shrink_to_fit();

        assert!(cache.map.is_empty(), "the cache should have drained");
        assert_eq!(
            cache.atlas.size().width,
            16,
            "a drained cache should give the atlas back to the floor"
        );
    }

    /// Shrinking may move residents, but must never drop one.
    #[test]
    fn shrinking_keeps_every_resident_image() {
        let mut cache = ImageCache::new_with_sizes(16, 256);
        let big = image(1, 64, 64);

        cache.begin_resolve();
        while cache.get_or_insert(&big).is_none() {
            assert!(cache.bump_size());
        }
        cache.get_or_insert(&big).unwrap();
        cache.shrink_to_fit();

        assert!(
            cache.map.contains_key(&big.data.id()),
            "a resident image was dropped by the shrink"
        );
        assert!(
            cache.atlas.size().width >= 64,
            "the atlas shrank below what its resident image needs"
        );
        // Moved or not, it must be re-uploaded before it is sampled again.
        assert!(
            cache.map[&big.data.id()].dirty,
            "a moved image must be marked for re-upload"
        );
    }

    /// A steady scene must not churn: nothing to give back means no work.
    #[test]
    fn shrinking_is_a_no_op_at_the_initial_size() {
        let mut cache = ImageCache::new_with_sizes(32, 256);
        let small = image(3, 8, 8);

        cache.begin_resolve();
        let before = cache.get_or_insert(&small).unwrap();
        cache.shrink_to_fit();

        assert_eq!(cache.atlas.size().width, 32);
        assert_eq!(
            cache.get_or_insert(&small).unwrap(),
            before,
            "a no-op shrink must not move a resident image"
        );
    }

    #[test]
    fn resident_entries_are_reused_across_resolves() {
        let mut cache = ImageCache::new_with_sizes(32, 64);
        let image = image(7, 8, 8);

        cache.begin_resolve();
        let first = cache.get_or_insert(&image).unwrap();
        assert_eq!(cache.images.len(), 1);
        cache.finish_resolve();

        cache.begin_resolve();
        let second = cache.get_or_insert(&image).unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.images.len(), 0);
        assert_eq!(cache.map.len(), 1);
    }

    #[test]
    fn marked_dirty_resident_entries_are_uploaded_again() {
        let mut cache = ImageCache::new_with_sizes(32, 64);
        let image = image(7, 8, 8);

        cache.begin_resolve();
        let first = cache.get_or_insert(&image).unwrap();
        cache.finish_resolve();

        cache.mark_dirty(&image);
        cache.begin_resolve();
        let second = cache.get_or_insert(&image).unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.images.len(), 1);
        assert_eq!(cache.images[0].0.data.id(), image.data.id());
        assert_eq!((cache.images[0].1, cache.images[0].2), first);
        cache.finish_resolve();

        cache.begin_resolve();
        assert_eq!(cache.get_or_insert(&image), Some(first));
        assert_eq!(cache.images.len(), 0);
    }

    #[test]
    fn marked_dirty_unused_entries_stay_dirty() {
        let mut cache = ImageCache::new_with_sizes(32, 64);
        let image = image(7, 8, 8);

        cache.begin_resolve();
        let xy = cache.get_or_insert(&image).unwrap();
        cache.finish_resolve();

        cache.mark_dirty(&image);
        cache.begin_resolve();
        cache.finish_resolve();

        cache.begin_resolve();
        assert_eq!(cache.get_or_insert(&image), Some(xy));
        assert_eq!(cache.images.len(), 1);
    }

    #[test]
    fn stale_entries_can_be_evicted_under_pressure() {
        let mut cache = ImageCache::new_with_sizes(16, 16);
        let image_a = image(1, 10, 10);
        let image_b = image(2, 10, 10);

        cache.begin_resolve();
        assert!(cache.get_or_insert(&image_a).is_some());

        cache.begin_resolve();

        cache.begin_resolve();
        cache.begin_resolve();
        assert!(cache.get_or_insert(&image_b).is_none());
        assert!(cache.evict_stale_entries());
        assert!(cache.get_or_insert(&image_b).is_some());
        assert!(!cache.map.contains_key(&image_a.data.id()));
    }

    #[test]
    fn stale_entries_are_evicted_at_most_once_per_resolve() {
        let mut cache = ImageCache::new_with_sizes(32, 32);
        let image_a = image(1, 8, 8);
        let image_b = image(2, 8, 8);

        cache.begin_resolve();
        assert!(cache.get_or_insert(&image_a).is_some());
        cache.finish_resolve();

        cache.begin_resolve();
        assert!(cache.get_or_insert(&image_b).is_some());
        cache.finish_resolve();

        cache.begin_resolve();
        cache.begin_resolve();
        cache.begin_resolve();

        assert!(cache.evict_stale_entries());
        assert!(!cache.evict_stale_entries());
    }

    #[test]
    fn failed_repack_leaves_existing_residency_unchanged() {
        let mut cache = ImageCache::new_with_sizes(16, 16);
        let image = image(1, 12, 12);

        cache.begin_resolve();
        let xy = cache.get_or_insert(&image).unwrap();

        assert!(!cache.repack_to_size(8));
        assert_eq!(cache.atlas.size().width, 16);
        assert_eq!(cache.get_or_insert(&image), Some(xy));
        assert_eq!(cache.map.len(), 1);
    }
}
