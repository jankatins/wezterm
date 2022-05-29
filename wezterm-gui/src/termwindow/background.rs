use crate::termwindow::RenderState;
use crate::Dimensions;
use anyhow::Context;
use config::{
    BackgroundLayer, BackgroundSize, BackgroundSource, ConfigHandle, GradientOrientation,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use termwiz::image::{ImageData, ImageDataType};
use wezterm_term::color::ColorPalette;

lazy_static::lazy_static! {
    static ref IMAGE_CACHE: Mutex<HashMap<String, CachedImage>> = Mutex::new(HashMap::new());
}

struct CachedImage {
    modified: SystemTime,
    image: Arc<ImageData>,
    marked: bool,
}

impl CachedImage {
    fn load(path: &str) -> anyhow::Result<Arc<ImageData>> {
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .with_context(|| format!("getting metadata for {}", path))?;
        let mut cache = IMAGE_CACHE.lock().unwrap();
        if let Some(cached) = cache.get_mut(path) {
            if cached.modified == modified {
                cached.marked = false;
                return Ok(Arc::clone(&cached.image));
            }
        }

        let data = std::fs::read(path)
            .with_context(|| format!("Failed to load window_background_image {}", path))?;
        log::trace!("loaded {}", path);
        let data = ImageDataType::EncodedFile(data).decode();
        let image = Arc::new(ImageData::with_data(data));

        cache.insert(
            path.to_string(),
            Self {
                modified,
                image: Arc::clone(&image),
                marked: false,
            },
        );

        Ok(image)
    }

    fn mark() {
        let mut cache = IMAGE_CACHE.lock().unwrap();
        for entry in cache.values_mut() {
            entry.marked = true;
        }
    }

    fn sweep() {
        let mut cache = IMAGE_CACHE.lock().unwrap();
        cache.retain(|k, entry| {
            if entry.marked {
                log::trace!("Unloading {} from cache", k);
            }
            !entry.marked
        });
    }
}

pub struct LoadedBackgroundLayer {
    pub source: Arc<ImageData>,
    pub def: BackgroundLayer,
}

fn load_background_layer(
    layer: &BackgroundLayer,
    dimensions: &Dimensions,
) -> anyhow::Result<LoadedBackgroundLayer> {
    let data = match &layer.source {
        BackgroundSource::Gradient(g) => {
            let grad = g
                .build()
                .with_context(|| format!("building gradient {:?}", g))?;

            let mut width = match layer.width {
                BackgroundSize::Percent(p) => (p as u32 * dimensions.pixel_width as u32) / 100,
                BackgroundSize::Length(u) => u as u32,
                unsup => anyhow::bail!("{:?} not yet implemented", unsup),
            };
            let mut height = match layer.height {
                BackgroundSize::Percent(p) => (p as u32 * dimensions.pixel_height as u32) / 100,
                BackgroundSize::Length(u) => u as u32,
                unsup => anyhow::bail!("{:?} not yet implemented", unsup),
            };

            if matches!(g.orientation, GradientOrientation::Radial { .. }) {
                // To simplify the math, we compute a perfect circle
                // for the radial gradient, and let the texture sampler
                // perturb it to fill the window
                width = width.min(height);
                height = height.min(width);
            }

            let mut imgbuf = image::RgbaImage::new(width, height);
            let fw = width as f64;
            let fh = height as f64;

            fn to_pixel(c: colorgrad::Color) -> image::Rgba<u8> {
                let (r, g, b, a) = c.rgba_u8();
                image::Rgba([r, g, b, a])
            }

            // Map t which is in range [a, b] to range [c, d]
            fn remap(t: f64, a: f64, b: f64, c: f64, d: f64) -> f64 {
                (t - a) * ((d - c) / (b - a)) + c
            }

            let (dmin, dmax) = grad.domain();

            let rng = fastrand::Rng::new();

            // We add some randomness to the position that we use to
            // index into the color gradient, so that we can avoid
            // visible color banding.  The default 64 was selected
            // because it it was the smallest value on my mac where
            // the banding wasn't obvious.
            let noise_amount = g.noise.unwrap_or_else(|| {
                if matches!(g.orientation, GradientOrientation::Radial { .. }) {
                    16
                } else {
                    64
                }
            });

            fn noise(rng: &fastrand::Rng, noise_amount: usize) -> f64 {
                if noise_amount == 0 {
                    0.
                } else {
                    rng.usize(0..noise_amount) as f64 * -1.
                }
            }

            match g.orientation {
                GradientOrientation::Horizontal => {
                    for (x, _, pixel) in imgbuf.enumerate_pixels_mut() {
                        *pixel = to_pixel(grad.at(remap(
                            x as f64 + noise(&rng, noise_amount),
                            0.0,
                            fw,
                            dmin,
                            dmax,
                        )));
                    }
                }
                GradientOrientation::Vertical => {
                    for (_, y, pixel) in imgbuf.enumerate_pixels_mut() {
                        *pixel = to_pixel(grad.at(remap(
                            y as f64 + noise(&rng, noise_amount),
                            0.0,
                            fh,
                            dmin,
                            dmax,
                        )));
                    }
                }
                GradientOrientation::Linear { angle } => {
                    let angle = angle.unwrap_or(0.0).to_radians();
                    for (x, y, pixel) in imgbuf.enumerate_pixels_mut() {
                        let (x, y) = (x as f64, y as f64);
                        let (x, y) = (x - fw / 2., y - fh / 2.);
                        let t = x * f64::cos(angle) - y * f64::sin(angle);
                        *pixel = to_pixel(grad.at(remap(
                            t + noise(&rng, noise_amount),
                            -fw / 2.,
                            fw / 2.,
                            dmin,
                            dmax,
                        )));
                    }
                }
                GradientOrientation::Radial { radius, cx, cy } => {
                    let radius = fw * radius.unwrap_or(0.5);
                    let cx = fw * cx.unwrap_or(0.5);
                    let cy = fh * cy.unwrap_or(0.5);

                    for (x, y, pixel) in imgbuf.enumerate_pixels_mut() {
                        let nx = noise(&rng, noise_amount);
                        let ny = noise(&rng, noise_amount);

                        let t = (nx + (x as f64 - cx).powi(2) + (ny + y as f64 - cy).powi(2))
                            .sqrt()
                            / radius;
                        *pixel = to_pixel(grad.at(t));
                    }
                }
            }

            let data = imgbuf.into_vec();
            Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                width, height, data,
            )))
        }
        BackgroundSource::File(path) => CachedImage::load(path)?,
    };

    Ok(LoadedBackgroundLayer {
        source: data,
        def: layer.clone(),
    })
}

pub fn load_background_image(
    config: &ConfigHandle,
    dimensions: &Dimensions,
) -> Vec<LoadedBackgroundLayer> {
    let mut layers = vec![];
    for layer in &config.background {
        let load_start = std::time::Instant::now();
        match load_background_layer(layer, dimensions) {
            Ok(layer) => {
                log::trace!("loaded layer in {:?}", load_start.elapsed());
                layers.push(layer);
            }
            Err(err) => {
                log::error!("Failed to load background: {:#}", err);
            }
        }
    }
    layers
}

pub fn reload_background_image(
    config: &ConfigHandle,
    existing: &[LoadedBackgroundLayer],
    dimensions: &Dimensions,
) -> Vec<LoadedBackgroundLayer> {
    // We want to reuse the existing version of the image where possible
    // so that the textures we may have cached can be re-used and so that
    // animation state can be preserved across the reload.
    let map: HashMap<_, _> = existing
        .iter()
        .map(|layer| (layer.source.hash(), &layer.source))
        .collect();

    CachedImage::mark();

    let result = load_background_image(config, dimensions)
        .into_iter()
        .map(|mut layer| {
            let hash = layer.source.hash();

            if let Some(existing) = map.get(&hash) {
                layer.source = Arc::clone(existing);
            }

            layer
        })
        .collect();

    CachedImage::sweep();

    result
}

impl crate::TermWindow {
    pub fn render_backgrounds(
        &self,
        gl_state: &RenderState,
        palette: &ColorPalette,
    ) -> anyhow::Result<()> {
        for (idx, layer) in self.window_background.iter().enumerate() {
            self.render_background(gl_state, palette, layer, idx)?;
        }
        Ok(())
    }

    fn render_background(
        &self,
        gl_state: &RenderState,
        palette: &ColorPalette,
        layer: &LoadedBackgroundLayer,
        layer_index: usize,
    ) -> anyhow::Result<()> {
        let render_layer = gl_state.layer_for_zindex(-127 + layer_index as i8)?;
        let vbs = render_layer.vb.borrow();
        let mut vb_mut0 = vbs[0].current_vb_mut();
        let mut layer0 = vbs[0].map(&mut vb_mut0);

        let color = palette.background.to_linear().mul_alpha(layer.def.opacity);

        let (sprite, next_due) = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_image(&layer.source, None)?;
        self.update_next_frame_time(next_due);
        let mut quad = layer0.allocate()?;
        quad.set_position(
            self.dimensions.pixel_width as f32 / -2.,
            self.dimensions.pixel_height as f32 / -2.,
            self.dimensions.pixel_width as f32 / 2.,
            self.dimensions.pixel_height as f32 / 2.,
        );
        quad.set_texture(sprite.texture_coords());
        quad.set_is_background_image();
        quad.set_hsv(Some(layer.def.hsb));
        quad.set_fg_color(color);

        Ok(())
    }
}
