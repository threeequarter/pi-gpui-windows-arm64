//! Mermaid diagram text rendered to images: merman, Zed's Rust Mermaid
//! backend, produces raster-safe SVG, and resvg rasterizes it with the system
//! fonts. The theme colors come from the active theme, as in Zed's
//! mermaid_render crate.

use std::sync::{Arc, LazyLock};

use gpui::{Hsla, Rgba, px, size};
use image::RgbaImage;
use resvg::tiny_skia::{IntSize, Pixmap, Transform};

use crate::ui::ThemeColors;
use crate::ui::raster_preview::{RasterPreview, raster_bytes};

/// Rasterize at twice the logical size, as gpui does for its SVG icons.
const RASTER_SCALE: f32 = 2.;

static SVG_OPTIONS: LazyLock<usvg::Options<'static>> = LazyLock::new(|| {
    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    usvg::Options {
        fontdb: Arc::new(fontdb),
        ..usvg::Options::default()
    }
});

pub(super) fn render(
    source: &str,
    colors: &ThemeColors,
    dark: bool,
    font_family: &str,
) -> Result<RasterPreview, String> {
    let svg = render_svg(source, colors, dark, font_family)?;
    rasterize(&svg)
}

fn render_svg(
    source: &str,
    colors: &ThemeColors,
    dark: bool,
    font_family: &str,
) -> Result<String, String> {
    let request = merman::SvgRequest {
        pipeline: Some(merman::svg::SvgPipeline::resvg_safe()),
        ..merman::SvgRequest::default()
    };
    let output = merman::Renderer::new()
        .with_engine(merman::Engine::new().with_site_config(config(colors, dark, font_family)))
        .render(merman::RenderRequest::svg(
            source,
            merman::OperationControl::new(),
            request,
        ))
        .map_err(|error| error.to_string())?;
    let merman::RenderOutput::Svg(Some(svg)) = output else {
        return Err("not a Mermaid diagram".into());
    };
    Ok(svg.svg().to_string())
}

fn rasterize(svg: &str) -> Result<RasterPreview, String> {
    let tree =
        usvg::Tree::from_data(svg.as_bytes(), &SVG_OPTIONS).map_err(|error| error.to_string())?;
    let natural = tree.size();
    let width = (natural.width() * RASTER_SCALE).ceil() as u32;
    let height = (natural.height() * RASTER_SCALE).ceil() as u32;
    let invalid_size = || "the diagram raster size is invalid".to_string();
    let dimensions = IntSize::from_wh(width, height).ok_or_else(invalid_size)?;
    if width > i32::MAX as u32 / 4 {
        return Err(invalid_size());
    }
    let mut pixmap =
        Pixmap::from_vec(raster_bytes(width, height)?, dimensions).ok_or_else(invalid_size)?;
    resvg::render(
        &tree,
        Transform::from_scale(RASTER_SCALE, RASTER_SCALE),
        &mut pixmap.as_mut(),
    );
    let mut bytes = pixmap.take();
    for pixel in bytes.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel.fill(0);
        } else {
            for channel in &mut pixel[..3] {
                *channel = (u16::from(*channel) * 255 / alpha).min(255) as u8;
            }
            pixel.swap(0, 2);
        }
    }
    let pixels = RgbaImage::from_raw(width, height, bytes).ok_or_else(invalid_size)?;
    RasterPreview::new(pixels, size(px(natural.width()), px(natural.height())))
}

fn config(colors: &ThemeColors, dark: bool, font_family: &str) -> merman::MermaidConfig {
    let primary = css_color(colors.element_background);
    let text = css_color(colors.text);
    let border = css_color(colors.border);
    let secondary = css_color(colors.panel_background);
    let tertiary = css_color(colors.ghost_element_hover);
    let background = css_color(colors.editor_background);
    let cluster_border = css_color(colors.border_variant);
    let branch_colors = [
        colors.syntax.keyword,
        colors.syntax.string,
        colors.syntax.number,
        colors.syntax.function,
        colors.syntax.r#type,
        colors.syntax.constant,
        colors.syntax.property,
        colors.syntax.tag,
    ]
    .map(css_color);

    // An array of pairs, not json!: a macro object this wide exceeds the
    // default recursion limit.
    let mut theme_variables = serde_json::Map::from_iter(
        [
            ("primaryColor", primary.clone()),
            ("primaryTextColor", text.clone()),
            ("primaryBorderColor", border.clone()),
            ("lineColor", border.clone()),
            ("secondaryColor", secondary.clone()),
            ("secondaryTextColor", text.clone()),
            ("tertiaryColor", tertiary.clone()),
            ("tertiaryTextColor", text.clone()),
            ("background", background.clone()),
            ("mainBkg", primary.clone()),
            ("nodeBorder", border.clone()),
            ("nodeTextColor", text.clone()),
            ("clusterBkg", secondary.clone()),
            ("clusterBorder", cluster_border.clone()),
            ("titleColor", text.clone()),
            ("edgeLabelBackground", background.clone()),
            ("textColor", text.clone()),
            ("fontFamily", font_family.to_string()),
            ("noteBkgColor", primary.clone()),
            ("noteBorderColor", cluster_border.clone()),
            ("noteTextColor", text.clone()),
            ("actorBkg", primary.clone()),
            ("actorBorder", border.clone()),
            ("actorTextColor", text.clone()),
            ("labelTextColor", text.clone()),
            ("loopTextColor", text.clone()),
            ("signalColor", text.clone()),
            ("signalTextColor", text.clone()),
            ("activationBkgColor", tertiary.clone()),
            ("activationBorderColor", border.clone()),
            ("classText", text.clone()),
            ("labelColor", text.clone()),
            ("attributeBackgroundColorOdd", primary.clone()),
            ("attributeBackgroundColorEven", secondary.clone()),
            ("pieTitleTextColor", text.clone()),
            ("pieSectionTextColor", text.clone()),
            ("pieLegendTextColor", text.clone()),
            ("pieStrokeColor", border.clone()),
            ("pieOuterStrokeColor", border.clone()),
            ("quadrant1Fill", primary.clone()),
            ("quadrant2Fill", secondary.clone()),
            ("quadrant3Fill", primary.clone()),
            ("quadrant4Fill", secondary.clone()),
            ("quadrant1TextFill", text.clone()),
            ("quadrant2TextFill", text.clone()),
            ("quadrant3TextFill", text.clone()),
            ("quadrant4TextFill", text.clone()),
            ("quadrantPointFill", border.clone()),
            ("quadrantPointTextFill", text.clone()),
            ("quadrantTitleFill", text.clone()),
            ("quadrantXAxisTextFill", text.clone()),
            ("quadrantYAxisTextFill", text.clone()),
            ("quadrantExternalBorderStrokeFill", border.clone()),
            ("quadrantInternalBorderStrokeFill", border),
        ]
        .map(|(key, value)| (key.to_string(), serde_json::Value::from(value))),
    );
    for (index, color) in branch_colors.iter().enumerate() {
        theme_variables.insert(format!("cScale{index}"), color.clone().into());
        theme_variables.insert(format!("cScaleLabel{index}"), text.clone().into());
        theme_variables.insert(format!("pie{}", index + 1), color.clone().into());
    }

    // Mermaid's defaults protect these keys from user input; the theme must
    // be allowed to set them.
    let mut secure = merman::generated::default_site_config()
        .as_value()
        .get("secure")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    secure.extend(["themeVariables", "fontSize"].map(serde_json::Value::from));

    merman::MermaidConfig::from_value(serde_json::json!({
        "theme": "base",
        "look": "classic",
        "secure": secure,
        "darkMode": dark,
        "fontFamily": font_family,
        // resvg cannot rasterize HTML foreignObject labels; the raster-safe
        // pipeline replaces them with wrapped native SVG text.
        "htmlLabels": true,
        "flowchart": {
            "htmlLabels": true,
            "padding": 16,
            "wrappingWidth": 200,
            "minNodeWidth": 0,
        },
        "state": {
            "wrappingWidth": 200,
            "minNodeWidth": 0,
        },
        "themeVariables": theme_variables,
    }))
}

fn css_color(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Theme;

    #[test]
    fn flowchart_renders_to_an_image() {
        let theme = Theme::ayu(true);
        let diagram = render(
            "flowchart LR\n  A[Start] --> B{Check}\n  B -->|yes| C[Done]",
            &theme.colors,
            true,
            "Segoe UI",
        )
        .expect("a flowchart should render");
        let size = diagram.images().next().expect("a raster tile").size(0);
        assert!(size.width.0 > 0 && size.height.0 > 0);
        assert!(diagram.size.width > px(0.) && diagram.size.height > px(0.));
    }

    #[test]
    fn unsupported_source_fails() {
        let theme = Theme::ayu(false);
        assert!(render("just prose, no diagram", &theme.colors, false, "Segoe UI").is_err());
    }
}
