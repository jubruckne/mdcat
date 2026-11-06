//! Sizing, scrolling, and click-to-expand for image documents.

use ratatui::layout::Rect;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol, FontSize, Resize, StatefulImage};

pub(crate) struct ImageView {
    pub path: std::path::PathBuf,
    expanded: bool,
    scaled: Option<image::DynamicImage>,
    protocol: Option<StatefulProtocol>,
    viewport: Option<(usize, Rect)>,
    hit_area: Rect,
}

impl ImageView {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            expanded: false,
            scaled: None,
            protocol: None,
            viewport: None,
            hit_area: Rect::default(),
        }
    }

    pub fn toggle_at(&mut self, x: u16, y: u16) -> bool {
        if !self.hit_area.contains((x, y).into()) {
            return false;
        }
        self.expanded = !self.expanded;
        true
    }

    /// Update the cached image size and return its full height in terminal rows.
    pub fn layout(&mut self, image: &image::DynamicImage, picker: &Picker, area: Rect) -> usize {
        let content = content_area(area);
        let font = picker.font_size();
        let max_width = u32::from(content.width.max(1)) * u32::from(font.width);
        let max_height = u32::from(content.height.max(1)) * u32::from(font.height);
        let scale = if self.expanded {
            f64::from(max_width) / f64::from(image.width())
        } else {
            (f64::from(max_width) / f64::from(image.width()))
                .min(f64::from(max_height) / f64::from(image.height()))
        };
        let dimensions = (
            (f64::from(image.width()) * scale).round().max(1.0) as u32,
            (f64::from(image.height()) * scale).round().max(1.0) as u32,
        );
        if self
            .scaled
            .as_ref()
            .map(|image| (image.width(), image.height()))
            != Some(dimensions)
        {
            self.scaled = Some(image.resize_exact(
                dimensions.0,
                dimensions.1,
                image::imageops::FilterType::Lanczos3,
            ));
            self.viewport = None;
        }
        dimensions.1.div_ceil(u32::from(font.height)) as usize
    }

    pub fn draw(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        picker: &Picker,
        area: Rect,
        scroll: usize,
    ) {
        let content = content_area(area);
        self.hit_area = Rect::default();
        if content.is_empty() {
            return;
        }
        let Some(image) = &self.scaled else { return };
        let FontSize { width, height } = picker.font_size();
        let top = (scroll as u64 * u64::from(height)).min(u64::from(image.height())) as u32;
        let pixels = (u32::from(content.height) * u32::from(height)).min(image.height() - top);
        if pixels == 0 {
            return;
        }
        self.hit_area = Rect::new(
            content.x,
            content.y,
            image
                .width()
                .div_ceil(u32::from(width))
                .min(u32::from(content.width)) as u16,
            pixels.div_ceil(u32::from(height)) as u16,
        );
        if self.viewport != Some((scroll, self.hit_area)) {
            // Crop the already scaled image so a tall image keeps its full width when scrolling.
            self.protocol =
                Some(picker.new_resize_protocol(image.crop_imm(0, top, image.width(), pixels)));
            self.viewport = Some((scroll, self.hit_area));
        }
        if let Some(protocol) = &mut self.protocol {
            frame.render_stateful_widget(
                StatefulImage::default().resize(Resize::Crop(None)),
                self.hit_area,
                protocol,
            );
        }
    }
}

fn content_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn draw(
        view: &mut ImageView,
        picker: &Picker,
        area: Rect,
        scroll: usize,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            area.right(),
            area.bottom(),
        ))
        .unwrap();
        terminal
            .draw(|frame| view.draw(frame, picker, area, scroll))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn click_toggles_window_fit_and_full_inner_width() {
        let picker = Picker::halfblocks();
        let bounds = Rect::new(0, 0, 100, 40);
        let image = image::DynamicImage::new_rgb8(800, 400);
        let mut view = ImageView::new("landscape.png".into());
        assert_eq!(view.layout(&image, &picker, bounds), 24);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 480));
        draw(&mut view, &picker, bounds, 0);
        assert_eq!(view.hit_area, Rect::new(2, 1, 96, 24));
        assert!(!view.toggle_at(1, 1)); // Margin.
        assert!(!view.toggle_at(98, 1)); // Right margin.
        assert!(view.toggle_at(2, 1));
        assert_eq!(view.layout(&image, &picker, bounds), 24);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 480));
        draw(&mut view, &picker, bounds, 0);
        assert_eq!(view.hit_area, Rect::new(2, 1, 96, 24));
        assert!(view.toggle_at(97, 24));
        view.layout(&image, &picker, bounds);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 480));
        // Fitted mode follows the current inner window on resize.
        let smaller = Rect::new(0, 0, 60, 20);
        assert_eq!(view.layout(&image, &picker, smaller), 14);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (560, 280));
        let wide = image::DynamicImage::new_rgb8(1600, 400);
        assert_eq!(view.layout(&wide, &picker, bounds), 12);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 240));
    }

    #[test]
    fn tall_expanded_image_scrolls_without_shrinking_to_viewport() {
        let picker = Picker::halfblocks();
        let area = Rect::new(0, 3, 100, 37); // Nested document title bars.
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(200, 1000, |_, y| {
            if y < 500 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        }));
        let mut view = ImageView::new("portrait.png".into());
        assert_eq!(view.layout(&image, &picker, area), 35);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (140, 700));
        draw(&mut view, &picker, area, 0);
        assert!(view.toggle_at(2, 4));
        let rows = view.layout(&image, &picker, area);
        assert_eq!(rows, 240);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 4800));
        let top = draw(&mut view, &picker, area, 0);
        let bottom = draw(
            &mut view,
            &picker,
            area,
            rows - usize::from(area.height - 2),
        );
        assert_eq!(view.hit_area, Rect::new(2, 4, 96, 35));
        assert_ne!(top[(2, 4)].fg, bottom[(2, 4)].fg);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 4800));
        assert!(view.toggle_at(2, 4));
        assert_eq!(view.layout(&image, &picker, area), 35);
    }

    #[test]
    fn small_images_scale_up_to_fit_and_follow_window_resize() {
        let picker = Picker::halfblocks();
        let bounds = Rect::new(0, 0, 100, 40);
        let image = image::DynamicImage::new_rgb8(8, 8);
        let mut view = ImageView::new("small.png".into());
        view.layout(&image, &picker, bounds);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (760, 760));
        draw(&mut view, &picker, bounds, 0);
        assert!(view.toggle_at(2, 1));
        view.layout(&image, &picker, bounds);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (960, 960));
        let smaller = Rect::new(0, 0, 60, 20);
        view.layout(&image, &picker, smaller);
        assert_eq!(view.scaled.as_ref().unwrap().dimensions(), (560, 560));
        draw(&mut view, &picker, smaller, 0);
        assert_eq!(view.hit_area, Rect::new(2, 1, 56, 18));
        let tiny = Rect::new(0, 0, 3, 1);
        view.layout(&image, &picker, tiny);
        draw(&mut view, &picker, tiny, 0);
        assert!(!view.toggle_at(2, 1));
    }
}
