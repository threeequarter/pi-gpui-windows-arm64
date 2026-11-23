use std::sync::Arc;

use gpui::{
    Bounds, Corners, Div, Pixels, RenderImage, Size, canvas, div, point, prelude::*, px, size,
};
use image::{Frame, RgbaImage};

const TILE_EDGE: u32 = 2048;

/// A raster split into tiles so its dimensions are independent of the GPU atlas limit.
pub struct RasterPreview {
    pub size: Size<Pixels>,
    raster_size: Size<Pixels>,
    tiles: Arc<[RasterTile]>,
}

struct RasterTile {
    bounds: Bounds<Pixels>,
    image: Arc<RenderImage>,
}

impl RasterPreview {
    pub fn new(pixels: RgbaImage, size: Size<Pixels>) -> Result<Self, String> {
        let (width, height) = pixels.dimensions();
        let raster_size = raster_size(width, height);
        let mut tiles = Vec::new();
        if width <= TILE_EDGE && height <= TILE_EDGE {
            tiles.push(RasterTile::new(0, 0, pixels));
        } else {
            for y in (0..height).step_by(TILE_EDGE as usize) {
                for x in (0..width).step_by(TILE_EDGE as usize) {
                    let tile_width = (width - x).min(TILE_EDGE);
                    let tile_height = (height - y).min(TILE_EDGE);
                    let mut bytes = raster_bytes(tile_width, tile_height)?;
                    let row_bytes = tile_width as usize * 4;
                    for (row, target) in bytes.chunks_exact_mut(row_bytes).enumerate() {
                        let start = ((y as usize + row) * width as usize + x as usize) * 4;
                        target.copy_from_slice(&pixels.as_raw()[start..start + row_bytes]);
                    }
                    let tile = RgbaImage::from_raw(tile_width, tile_height, bytes)
                        .ok_or_else(|| "invalid preview tile size".to_string())?;
                    tiles.push(RasterTile::new(x, y, tile));
                }
            }
        }
        Ok(Self {
            size,
            raster_size,
            tiles: tiles.into(),
        })
    }

    pub fn images(&self) -> impl Iterator<Item = Arc<RenderImage>> + '_ {
        self.tiles.iter().map(|tile| tile.image.clone())
    }

    pub fn fit_scale(&self, available: Size<Pixels>) -> f32 {
        (available.width / self.size.width)
            .min(available.height / self.size.height)
            .min(1.)
    }

    pub fn render(&self, scale: Option<f32>) -> Div {
        let tiles = self.tiles.clone();
        let natural = self.size;
        let raster_size = self.raster_size;
        div()
            .flex_none()
            .map(|this| match scale {
                Some(scale) => this.w(natural.width * scale).h(natural.height * scale),
                None => this.size_full(),
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let fitted = match scale {
                            Some(_) => bounds,
                            None => {
                                let fit = (bounds.size.width / natural.width)
                                    .min(bounds.size.height / natural.height)
                                    .min(1.);
                                let fitted = natural.map(|edge| edge * fit);
                                Bounds {
                                    origin: bounds.origin
                                        + point(
                                            (bounds.size.width - fitted.width) * 0.5,
                                            (bounds.size.height - fitted.height) * 0.5,
                                        ),
                                    size: fitted,
                                }
                            }
                        };
                        let horizontal = fitted.size.width / raster_size.width;
                        let vertical = fitted.size.height / raster_size.height;
                        let clip = window.content_mask().bounds;
                        for tile in tiles.iter() {
                            let tile_bounds = Bounds {
                                origin: fitted.origin
                                    + point(
                                        tile.bounds.origin.x * horizontal,
                                        tile.bounds.origin.y * vertical,
                                    ),
                                size: size(
                                    tile.bounds.size.width * horizontal,
                                    tile.bounds.size.height * vertical,
                                ),
                            };
                            if !clip.intersects(&tile_bounds) {
                                continue;
                            }
                            if let Err(error) = window.paint_image(
                                tile_bounds,
                                Corners::default(),
                                tile.image.clone(),
                                0,
                                false,
                            ) {
                                eprintln!("Could not paint preview tile: {error}");
                            }
                        }
                    },
                )
                .size_full(),
            )
    }
}

impl RasterTile {
    fn new(x: u32, y: u32, pixels: RgbaImage) -> Self {
        Self {
            bounds: Bounds {
                origin: point(px(x as f32), px(y as f32)),
                size: raster_size(pixels.width(), pixels.height()),
            },
            image: Arc::new(RenderImage::new(vec![Frame::new(pixels)])),
        }
    }
}

pub fn raster_size(width: u32, height: u32) -> Size<Pixels> {
    size(px(width as f32), px(height as f32))
}

pub fn raster_bytes(width: u32, height: u32) -> Result<Vec<u8>, String> {
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "invalid preview raster size".to_string())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|error| error.to_string())?;
    bytes.resize(len, 0);
    Ok(bytes)
}
