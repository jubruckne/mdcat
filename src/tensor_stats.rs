//! Cancellable statistics over tensors and rows or columns of a slice.
use anyhow::{Context, Result};
use mdcat::gguf_view::TensorData;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub const HOVER_DELAY: Duration = Duration::from_millis(250);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const BINS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Tensor { planes: u64 },
    Row(u64),
    Column(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub axis: Axis,
    pub plane: u64,
    pub quantized: bool,
    // Logical value dimensions, excluding interleaved block metadata columns.
    pub rows: u64,
    pub columns: u64,
}
impl Target {
    pub fn count(self) -> u64 {
        match self.axis {
            Axis::Tensor { planes } => planes * self.rows * self.columns,
            Axis::Row(_) => self.columns,
            Axis::Column(_) => self.rows,
        }
    }
    fn number(self, data: &TensorData, index: u64) -> Result<f64> {
        if let (true, Axis::Column(col)) = (self.quantized, self.axis) {
            let layout = data
                .quantization()
                .context("Tensor has no quantization blocks")?;
            let block_len = layout.block_len as u64;
            let fields = block_len + layout.metadata.len() as u64;
            let block =
                (self.plane * self.rows + index) * (self.columns / block_len) + col / fields;
            return data.quantized_number(block, (col % fields) as usize);
        }

        let flat = match self.axis {
            Axis::Tensor { .. } => index,
            Axis::Row(row) => (self.plane * self.rows + row) * self.columns + index,
            Axis::Column(col) => (self.plane * self.rows + index) * self.columns + col,
        };
        if !self.quantized {
            return data.number(flat);
        }
        // Aggregate codes separately from scales and offsets.
        let layout = data
            .quantization()
            .context("Tensor has no quantization blocks")?;
        let block_len = layout.block_len as u64;
        data.quantized_number(
            flat / block_len,
            layout.metadata.len() + (flat % block_len) as usize,
        )
    }
}

#[derive(Debug)]
pub struct Statistics {
    pub total: u64,
    pub finite: u64,
    pub minimum: f64,
    pub maximum: f64,
    pub mean: f64,
    pub histogram: [u64; BINS],
    pub metadata: Vec<(&'static str, Statistics)>,
}
impl Statistics {
    #[cfg(test)]
    pub(crate) fn calculate(
        target: Target,
        data: &TensorData,
        cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        Self::scan(target, data, cancel, &AtomicU64::new(0))
    }

    fn scan(
        target: Target,
        data: &TensorData,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<Option<Self>> {
        let Some(mut result) = Self::collect(target.count(), cancel, Some(progress), |i| {
            target.number(data, i)
        })?
        else {
            return Ok(None);
        };
        if target.quantized {
            if !matches!(target.axis, Axis::Column(_)) {
                let layout = data
                    .quantization()
                    .context("Tensor has no quantization blocks")?;
                let blocks_per_row = target.columns / layout.block_len as u64;
                let (first, count) = match target.axis {
                    Axis::Row(row) => (
                        (target.plane * target.rows + row) * blocks_per_row,
                        blocks_per_row,
                    ),
                    Axis::Tensor { .. } => (0, target.count() / layout.block_len as u64),
                    Axis::Column(_) => unreachable!(),
                };
                for (field, label) in layout.metadata.iter().enumerate() {
                    let label = match *label {
                        "Scale" => "Scales",
                        "Min" => "Offsets",
                        _ => continue,
                    };
                    let Some(stats) = Self::collect(count, cancel, None, |i| {
                        data.quantized_number(first + i, field)
                    })?
                    else {
                        return Ok(None);
                    };
                    result.metadata.push((label, stats));
                }
                let subscales: Vec<_> = layout
                    .metadata
                    .iter()
                    .enumerate()
                    .filter_map(|(i, label)| label.starts_with("Subscale").then_some(i))
                    .collect();
                if !subscales.is_empty() {
                    let n = subscales.len() as u64;
                    let Some(stats) = Self::collect(count * n, cancel, None, |i| {
                        data.quantized_number(first + i / n, subscales[(i % n) as usize])
                    })?
                    else {
                        return Ok(None);
                    };
                    result.metadata.push(("Subscales", stats));
                }
            }
        }
        Ok(Some(result))
    }
    fn collect(
        total: u64,
        cancel: &AtomicBool,
        progress: Option<&AtomicU64>,
        number: impl Fn(u64) -> Result<f64>,
    ) -> Result<Option<Self>> {
        let mut result = Self {
            total,
            finite: 0,
            minimum: f64::INFINITY,
            maximum: f64::NEG_INFINITY,
            mean: 0.0,
            histogram: [0; BINS],
            metadata: Vec::new(),
        };
        for i in 0..result.total {
            if i % 256 == 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                if let Some(progress) = progress {
                    progress.store(i, Ordering::Relaxed);
                }
            }
            let value = number(i)?;
            if !value.is_finite() {
                continue;
            }
            result.finite += 1;
            result.minimum = result.minimum.min(value);
            result.maximum = result.maximum.max(value);
            let weight = 1.0 / result.finite as f64;
            result.mean = result.mean * (1.0 - weight) + value * weight;
        }
        for i in 0..result.total {
            if i % 256 == 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                if let Some(progress) = progress {
                    progress.store(total.saturating_add(i), Ordering::Relaxed);
                }
            }
            let value = number(i)?;
            if !value.is_finite() {
                continue;
            }
            let fraction = if result.minimum == result.maximum {
                0.0
            } else {
                let range = result.maximum - result.minimum;
                if range.is_finite() {
                    (value - result.minimum) / range
                } else {
                    (value * 0.5 - result.minimum * 0.5)
                        / (result.maximum * 0.5 - result.minimum * 0.5)
                }
            };
            let bin = ((fraction * BINS as f64) as usize).min(BINS - 1);
            result.histogram[bin] += 1;
        }
        if let Some(progress) = progress {
            progress.store(total.saturating_mul(2), Ordering::Relaxed);
        }
        Ok(Some(result))
    }
}

type Outcome = std::result::Result<Arc<Statistics>, String>;
#[derive(Default)]
pub struct Hover {
    target: Option<Target>,
    started: Option<Instant>,
    result: Option<Outcome>,
    receiver: Option<mpsc::Receiver<Outcome>>,
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<Arc<AtomicU64>>,
    progress_updated: Option<Instant>,
    displayed_progress: u64,
    cache: VecDeque<(Target, Arc<Statistics>)>,
}
impl Drop for Hover {
    fn drop(&mut self) {
        self.cancel_job();
    }
}
impl Hover {
    fn cancel_job(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.receiver = None;
        self.progress = None;
        self.progress_updated = None;
        self.displayed_progress = 0;
    }
    pub fn update(&mut self, target: Option<Target>, data: &Arc<TensorData>, now: Instant) {
        if target != self.target {
            self.cancel_job();
            self.target = target;
            self.started = Some(now);
            self.result = None;
            if let Some(target) = target {
                if let Some(index) = self.cache.iter().position(|(key, _)| *key == target) {
                    let entry = self.cache.remove(index).unwrap();
                    self.result = Some(Ok(Arc::clone(&entry.1)));
                    self.cache.push_back(entry);
                }
            }
        }
        let Some(target) = self.target else {
            return;
        };
        if let Some(progress) = &self.progress {
            if self
                .progress_updated
                .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
            {
                self.progress_updated = Some(now);
                self.displayed_progress = progress.load(Ordering::Relaxed);
            }
        }
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(result) => {
                    if let Ok(stats) = &result {
                        self.cache.push_back((target, Arc::clone(stats)));
                        if self.cache.len() > 16 {
                            self.cache.pop_front();
                        }
                    }
                    self.result = Some(result);
                    self.receiver = None;
                    self.cancel = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("Statistics worker stopped".into()));
                    self.receiver = None;
                    self.cancel = None;
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if self.result.is_none() && self.receiver.is_none() && self.visible(now) {
            let (sender, receiver) = mpsc::channel();
            let data = Arc::clone(data);
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = Arc::clone(&cancel);
            let progress = Arc::new(AtomicU64::new(0));
            let worker_progress = Arc::clone(&progress);
            match std::thread::Builder::new()
                .name("mdcat-tensor-stats".into())
                .spawn(move || {
                    let result =
                        match Statistics::scan(target, &data, &worker_cancel, &worker_progress) {
                            Ok(Some(stats)) => Ok(Arc::new(stats)),
                            Ok(None) => return,
                            Err(error) => Err(format!("{error:#}")),
                        };
                    let _ = sender.send(result);
                }) {
                Ok(_) => {
                    self.receiver = Some(receiver);
                    self.cancel = Some(cancel);
                    self.progress = Some(progress);
                    self.progress_updated = Some(now);
                }
                Err(error) => {
                    self.result = Some(Err(format!("Could not start statistics worker: {error}")))
                }
            }
        }
    }
    pub fn target(&self) -> Option<Target> {
        self.target
    }
    pub fn visible(&self, now: Instant) -> bool {
        self.target.is_some()
            && self
                .started
                .is_some_and(|start| now.saturating_duration_since(start) >= HOVER_DELAY)
    }
    pub fn needs_tick(&self) -> bool {
        self.target.is_some() && (!self.visible(Instant::now()) || self.result.is_none())
    }
    pub fn progress_percent(&self) -> u64 {
        self.target.map_or(0, |target| {
            (100 * u128::from(self.displayed_progress) / (2 * u128::from(target.count()))) as u64
        })
    }
    pub fn result(&self) -> Option<&Outcome> {
        self.result.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_row_and_column_statistics_include_offscreen_values() {
        let (_dir, path) = crate::tensor_view::tests::fixture();
        let data = TensorData::open(&path, 0).unwrap();
        let row = Target {
            axis: Axis::Row(1),
            plane: 1,
            quantized: false,
            rows: 3,
            columns: 4,
        };
        let stats = Statistics::calculate(row, &data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(
            (stats.total, stats.finite, stats.minimum, stats.maximum),
            (4, 4, 16.0, 19.0)
        );
        assert!((stats.mean - 17.5).abs() < 1e-12);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 4);
        let column = Target {
            axis: Axis::Column(2),
            ..row
        };
        let stats = Statistics::calculate(column, &data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!((stats.total, stats.minimum, stats.maximum), (3, 14.0, 22.0));
        assert!((stats.mean - 18.0).abs() < 1e-12);
        assert!(Statistics::calculate(column, &data, &AtomicBool::new(true))
            .unwrap()
            .is_none());
    }

    #[test]
    fn quantized_row_statistics_exclude_scales_and_scale_columns_remain_separate() {
        let (_dir, path) = crate::tensor_view::tests::quantized_fixture(false);
        let data = TensorData::open(&path, 0).unwrap();
        let row = Target {
            axis: Axis::Row(0),
            plane: 0,
            quantized: true,
            rows: 2,
            columns: 64,
        };
        let stats = Statistics::calculate(row, &data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(
            (stats.total, stats.minimum, stats.maximum),
            (64, -16.0, 15.0)
        );
        assert!((stats.mean + 0.5).abs() < 1e-12);
        let scales = Statistics::calculate(
            Target {
                axis: Axis::Column(0),
                ..row
            },
            &data,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (scales.total, scales.minimum, scales.maximum, scales.mean),
            (2, 15360.0, 16896.0, 16128.0)
        );
        let codes = Statistics::calculate(
            Target {
                axis: Axis::Column(1),
                ..row
            },
            &data,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (codes.minimum, codes.maximum, codes.mean),
            (-16.0, -16.0, -16.0)
        );
    }

    #[test]
    fn non_finite_values_are_counted_but_excluded_from_aggregates() {
        let (_dir, path) = crate::tensor_view::tests::fixture();
        let mut bytes = std::fs::read(&path).unwrap();
        let payload = bytes.len() - 24 * 4;
        bytes[payload..payload + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let data = TensorData::open(&path, 0).unwrap();
        let target = Target {
            axis: Axis::Row(0),
            plane: 0,
            quantized: false,
            rows: 3,
            columns: 4,
        };
        let stats = Statistics::calculate(target, &data, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                stats.total,
                stats.finite,
                stats.minimum,
                stats.maximum,
                stats.mean
            ),
            (4, 3, 1.0, 3.0, 2.0)
        );
        assert_eq!(stats.histogram.iter().sum::<u64>(), 3);
    }
}
