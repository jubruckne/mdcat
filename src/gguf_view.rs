//! Read GGUF metadata and tensor descriptors without loading tensor payloads.
use anyhow::{Context, Result};
use std::{fs::File, io::Read, path::Path, sync::Arc};

// Own the mapping so GGUFContainer can consume a boxed reader without copying it.
struct MappedReader {
    mapping: Arc<memmap2::Mmap>,
    position: usize,
}
impl Read for MappedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let remaining = &self.mapping[self.position..];
        let count = out.len().min(remaining.len());
        out[..count].copy_from_slice(&remaining[..count]);
        self.position += count;
        Ok(count)
    }
}

#[allow(unsafe_code)]
fn open_model(path: &Path) -> Result<(gguf_rs::GGUFModel, Arc<memmap2::Mmap>, bool)> {
    let file =
        File::open(path).with_context(|| format!("Could not open GGUF file {}", path.display()))?;
    // Model files must remain unchanged during this short descriptor read.
    let mapping = Arc::new(unsafe { memmap2::Mmap::map(&file) }.context("Failed to map GGUF")?);
    let order = match mapping.get(..4) {
        Some(b"GGUF") => gguf_rs::ByteOrder::LE,
        Some(b"FUGG") => gguf_rs::ByteOrder::BE,
        _ => anyhow::bail!("Invalid GGUF magic"),
    };
    let mut container = gguf_rs::GGUFContainer::new(
        order,
        Box::new(MappedReader {
            mapping: Arc::clone(&mapping),
            position: 4,
        }),
        u64::MAX,
    );
    let model = container.decode().context("Failed to read GGUF")?;
    let little = &mapping[..4] == b"GGUF";
    Ok((model, mapping, little))
}

/// Render GGUF metadata and tensor descriptors as Markdown.
pub fn render(path: &Path) -> Result<String> {
    let (model, _, _) = open_model(path)?;
    let mut out =
        String::from("| Tensor | Shape | Type | Size (bytes) |\n| --- | ---: | --- | ---: |\n");
    for (index, tensor) in model.tensors().iter().enumerate() {
        let name = tensor
            .name
            .replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace(['\n', '\r'], " ");
        let shape = tensor
            .shape
            .iter()
            .rev()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" × ");
        let kind = gguf_rs::GGMLType::try_from(tensor.kind)?;
        let size = grouped_size(tensor_size(tensor.kind, &tensor.shape)?);
        out.push_str(&format!(
            "| [{name}](mdcat-tensor:{index}) | {shape} | {kind} | {size} |\n"
        ));
    }
    out.push_str("\n## Metadata\n\n```json\n");
    out.push_str(&serde_json::to_string_pretty(model.metadata())?);
    out.push_str("\n```\n");
    Ok(out)
}

fn grouped_size(size: u64) -> String {
    let digits = size.to_string();
    let mut result = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

// gguf-rs 0.1.8 uses incorrect block geometry for several types, including
// MXFP4 and scalar integers. Use GGML's storage geometry for display sizes.
// https://github.com/ggml-org/ggml/blob/master/src/ggml-common.h
fn tensor_size(kind: u32, shape: &[u64]) -> Result<u64> {
    let (block, bytes): (u64, u64) = match kind {
        0 => (1, 4),
        1 | 30 => (1, 2),
        2 | 20 => (32, 18),
        3 => (32, 20),
        6 => (32, 22),
        7 => (32, 24),
        8 => (32, 34),
        9 => (32, 36),
        10 => (256, 84),
        11 | 21 => (256, 110),
        12 => (256, 144),
        13 => (256, 176),
        14 => (256, 210),
        15 => (256, 292),
        16 => (256, 66),
        17 => (256, 74),
        18 => (256, 98),
        19 => (256, 50),
        22 => (256, 82),
        23 => (256, 136),
        24 => (1, 1),
        25 => (1, 2),
        26 => (1, 4),
        27 | 28 => (1, 8),
        29 => (256, 56),
        34 => (256, 54),
        35 => (256, 66),
        39 => (32, 17),
        _ => anyhow::bail!("Unsupported GGUF tensor type: {kind}"),
    };
    let elements = shape
        .iter()
        .try_fold(1u64, |n, dim| n.checked_mul(*dim))
        .context("Tensor shape overflow")?;
    anyhow::ensure!(
        shape.first().is_some_and(|dim| dim % block == 0),
        "Invalid quantization block shape"
    );
    (elements / block)
        .checked_mul(bytes)
        .context("Tensor size overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_descriptors_and_metadata() {
        fn string(out: &mut Vec<u8>, value: &str) {
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        string(&mut bytes, "general.name");
        bytes.extend_from_slice(&8u32.to_le_bytes());
        string(&mut bytes, "Example");
        string(&mut bytes, "weights");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&32u64.to_le_bytes());
        bytes.extend_from_slice(&4u64.to_le_bytes());
        bytes.extend_from_slice(&39u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.resize(bytes.len().next_multiple_of(32) + 96, 0);
        std::fs::write(&path, bytes).unwrap();
        let output = render(&path).unwrap();
        assert!(
            output.contains("[weights](mdcat-tensor:0) | 4 × 32 | MXFP4 | 68"),
            "{output}"
        );
        assert!(output
            .contains("| Tensor | Shape | Type | Size (bytes) |\n| --- | ---: | --- | ---: |"));
        assert_eq!(grouped_size(1_232_128), "1,232,128");
        assert!(output.contains("\"general.name\": \"Example\""));
        let data = TensorData::open(&path, 0).unwrap();
        assert_eq!(data.shape, vec![4, 32]);
        assert_eq!(data.value(127).unwrap(), "0");
        assert!(data.value(128).is_err());
        let length = std::fs::metadata(&path).unwrap().len();
        drop(data);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(length - 40)
            .unwrap();
        assert!(TensorData::open(&path, 0).is_err());
    }
    #[test]
    fn real_iq4_xs_model_if_requested() {
        let Some(path) = std::env::var_os("MDCAT_IQ4_XS_TEST_FILE") else {
            return;
        };
        let path = Path::new(&path);
        let (model, _, _) = open_model(path).unwrap();
        let mut checked = 0;
        for (index, descriptor) in model.tensors().iter().enumerate() {
            if descriptor.kind != 23 {
                continue;
            }
            let data = TensorData::open(path, index).unwrap();
            for i in [0, 15, 16, 31, 32, 255, 256, data.elements - 1] {
                assert!(data.number(i).unwrap().is_finite());
            }
            assert_eq!(data.quantization().unwrap().block_len, 256);
            for block in [0, data.elements / 256 - 1] {
                for field in 0..265 {
                    let value = data.quantized_number(block, field).unwrap();
                    assert!(
                        value >= 0.
                            && value
                                <= if field == 0 {
                                    65535.
                                } else if field < 9 {
                                    63.
                                } else {
                                    15.
                                }
                    );
                }
            }
            checked += 1;
        }
        assert!(checked > 0);
        eprintln!("Checked {checked} IQ4_XS tensors in {}", path.display());
    }

    #[test]
    fn real_model_if_requested() {
        if let Some(path) = std::env::var_os("MDCAT_GGUF_TEST_FILE") {
            let output = render(Path::new(&path)).unwrap();
            assert!(output.contains("MXFP4"));
            assert!(output.contains("tokenizer.ggml.tokens"));
            eprintln!("rendered {} bytes", output.len());
            for index in [0, 1, 20] {
                let data = TensorData::open(Path::new(&path), index).unwrap();
                eprintln!(
                    "{} {} {:?}: {}",
                    data.name,
                    data.kind,
                    data.shape,
                    data.value(0).unwrap()
                );
            }
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn mapped_quantized_values_follow_ggml_block_layout() {
        let file = tempfile::tempfile().unwrap();
        let mut bytes = vec![0u8; 128];
        // MXFP4 scale 1/2, low nibbles 0..15, high nibbles 15..0.
        bytes[0] = 127;
        for j in 0..16 {
            bytes[1 + j] = j as u8 | ((15 - j) as u8) << 4;
        }
        // Q8_0: half scale 2, signed samples -16..15.
        bytes[32..34].copy_from_slice(&half::f16::from_f32(2.0).to_bits().to_le_bytes());
        for j in 0..32 {
            bytes[34 + j] = (j as i8 - 16) as u8;
        }
        // I64 preserves values beyond f64's exact range.
        bytes[72..80].copy_from_slice(&i64::MAX.to_le_bytes());
        use std::io::{Seek, Write};
        let mut file = file;
        file.write_all(&bytes).unwrap();
        file.rewind().unwrap();
        let mapping = Arc::new(unsafe { memmap2::Mmap::map(&file).unwrap() });
        let mut data = TensorData {
            mapping,
            offset: 0,
            name: "test".into(),
            shape: vec![32],
            kind: "MXFP4".into(),
            type_id: 39,
            little: true,
            elements: 32,
        };
        let expected = [
            0., 0.5, 1., 1.5, 2., 3., 4., 6., 0., -0.5, -1., -1.5, -2., -3., -4., -6.,
        ];
        assert_eq!(data.quantized_field(0, 0).unwrap(), "127");
        for j in 0..16 {
            assert_eq!(
                data.quantized_field(0, j as usize + 1).unwrap(),
                j.to_string()
            );
            assert_eq!(
                data.quantized_field(0, j as usize + 17).unwrap(),
                (15 - j).to_string()
            );
            assert_eq!(
                data.value(j).unwrap().parse::<f32>().unwrap(),
                expected[j as usize]
            );
            assert_eq!(
                data.value(j + 16).unwrap().parse::<f32>().unwrap(),
                expected[15 - j as usize]
            );
        }
        data.offset = 32;
        data.type_id = 8;
        assert_eq!(data.quantized_field(0, 0).unwrap(), "16384");
        for j in 0..32 {
            assert_eq!(
                data.quantized_field(0, j as usize + 1).unwrap(),
                (j as i32 - 16).to_string()
            );
            assert_eq!(data.value(j).unwrap(), ((j as i32 - 16) * 2).to_string());
        }
        data.offset = 72;
        data.type_id = 27;
        data.elements = 1;
        assert_eq!(data.value(0).unwrap(), i64::MAX.to_string());
        assert!(data.quantization().is_none());
    }
}

/// A tensor backed by a read-only mapping. Only requested values are decoded.
pub struct TensorData {
    mapping: Arc<memmap2::Mmap>,
    offset: usize,
    /// Tensor name.
    pub name: String,
    /// Dimensions in PyTorch order (last dimension is contiguous).
    pub shape: Vec<u64>,
    /// GGML storage type name.
    pub kind: String,
    type_id: u32,
    little: bool,
    elements: u64,
}

enum TensorValue {
    Integer(i64),
    Float(f32),
    Double(f64),
}

impl TensorData {
    /// Read a numeric value for statistics without display rounding.
    /// Integer values are converted to f64 for aggregation.
    pub fn number(&self, index: u64) -> Result<f64> {
        Ok(match self.decode(index)? {
            TensorValue::Integer(value) => value as f64,
            TensorValue::Float(value) => f64::from(value),
            TensorValue::Double(value) => value,
        })
    }

    /// Read a value, using six decimal places for floating-point tensor formats.
    pub fn formatted_value(&self, index: u64) -> Result<String> {
        Ok(match self.decode(index)? {
            TensorValue::Integer(value) => value.to_string(),
            TensorValue::Float(value) => format!("{value:.6}"),
            TensorValue::Double(value) => format!("{value:.6}"),
        })
    }

    /// Read a scalar or dequantize one value, preserving integer formatting.
    pub fn value(&self, index: u64) -> Result<String> {
        Ok(match self.decode(index)? {
            TensorValue::Integer(value) => value.to_string(),
            TensorValue::Float(value) => value.to_string(),
            TensorValue::Double(value) => value.to_string(),
        })
    }

    /// Open a descriptor by its file-order index, checking its payload range.
    pub fn open(path: &Path, index: usize) -> Result<Self> {
        let (model, mapping, little) = open_model(path)?;
        let tensor = model
            .tensors()
            .get(index)
            .context("Tensor index out of range")?;
        anyhow::ensure!(
            matches!(
                tensor.kind,
                0 | 1 | 2 | 3 | 6 | 7 | 8 | 9 | 23 | 24..=28 | 30 | 39
            ),
            "Value exploration is not yet supported for {}",
            gguf_rs::GGMLType::try_from(tensor.kind)?
        );
        let size = tensor_size(tensor.kind, &tensor.shape)?;
        let offset = model
            .data_offset()
            .checked_add(tensor.offset)
            .context("Tensor offset overflow")?;
        let end = offset.checked_add(size).context("Tensor range overflow")?;
        anyhow::ensure!(end <= mapping.len() as u64, "Tensor payload is truncated");
        let elements = tensor
            .shape
            .iter()
            .try_fold(1u64, |n, d| n.checked_mul(*d))
            .context("Tensor size overflow")?;
        anyhow::ensure!(elements > 0, "Tensor is empty");
        Ok(Self {
            mapping,
            offset: usize::try_from(offset)?,
            name: tensor.name.clone(),
            shape: tensor.shape.iter().rev().copied().collect(),
            kind: gguf_rs::GGMLType::try_from(tensor.kind)?.to_string(),
            type_id: tensor.kind,
            little,
            elements,
        })
    }

    fn decode(&self, index: u64) -> Result<TensorValue> {
        anyhow::ensure!(index < self.elements, "Tensor index out of range");
        let (block, size) = match self.type_id {
            0 | 26 => (1, 4),
            1 | 25 | 30 => (1, 2),
            24 => (1, 1),
            27 | 28 => (1, 8),
            2 => (32, 18),
            3 => (32, 20),
            6 => (32, 22),
            7 => (32, 24),
            8 => (32, 34),
            9 => (32, 36),
            39 => (32, 17),
            23 => (256, 136),
            _ => anyhow::bail!("Unsupported tensor type"),
        };
        let start = self.offset + usize::try_from((index / block) * size)?;
        let b = self
            .mapping
            .get(start..start + size as usize)
            .context("Truncated tensor block")?;
        let u16_at = |offset: usize| {
            let a = [b[offset], b[offset + 1]];
            if self.little {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }
        };
        let u32_at = |offset: usize| {
            let a: [u8; 4] = b[offset..offset + 4].try_into().unwrap();
            if self.little {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }
        };
        let u64_at = || {
            let a: [u8; 8] = b.try_into().unwrap();
            if self.little {
                u64::from_le_bytes(a)
            } else {
                u64::from_be_bytes(a)
            }
        };
        let half_at = |offset| half::f16::from_bits(u16_at(offset)).to_f32();
        let j = (index % block) as usize;
        let value = match self.type_id {
            0 => f32::from_bits(u32_at(0)),
            1 => half_at(0),
            30 => half::bf16::from_bits(u16_at(0)).to_f32(),
            24 => return Ok(TensorValue::Integer(i64::from(b[0] as i8))),
            25 => return Ok(TensorValue::Integer(i64::from(u16_at(0) as i16))),
            26 => return Ok(TensorValue::Integer(i64::from(u32_at(0) as i32))),
            27 => return Ok(TensorValue::Integer(u64_at() as i64)),
            28 => return Ok(TensorValue::Double(f64::from_bits(u64_at()))),
            8 => half_at(0) * (b[2 + j] as i8) as f32,
            9 => half_at(0) * (b[4 + j] as i8) as f32,
            2 | 3 | 6 | 7 => {
                let packed = match self.type_id {
                    2 => 2,
                    3 => 4,
                    6 => 6,
                    _ => 8,
                };
                let nibble = if j < 16 {
                    b[packed + j] & 15
                } else {
                    b[packed + j - 16] >> 4
                };
                let q = if self.type_id == 6 || self.type_id == 7 {
                    let high = u32_at(packed - 4);
                    nibble | (((high >> j) & 1) as u8) << 4
                } else {
                    nibble
                };
                let minimum = if self.type_id == 3 || self.type_id == 7 {
                    half_at(2)
                } else {
                    0.0
                };
                let bias = match self.type_id {
                    2 => 8,
                    6 => 16,
                    _ => 0,
                };
                half_at(0) * (q as i32 - bias) as f32 + minimum
            }
            23 => {
                // GGML dequantize_row_iq4_xs: eight 32-value nonlinear groups.
                // https://github.com/ggml-org/ggml/blob/master/src/ggml-quants.c
                let values = [
                    -127., -104., -83., -65., -49., -35., -22., -10., 1., 13., 25., 38., 53., 69.,
                    89., 113.,
                ];
                let group = j / 32;
                let scale = iq4_xs_subscale(b, u16_at(2), group) as i32 - 32;
                let packed = b[8 + group * 16 + j % 16];
                let code = if j % 32 < 16 {
                    packed & 15
                } else {
                    packed >> 4
                };
                (half_at(0) * scale as f32) * values[code as usize]
            }
            39 => {
                // GGML's E2M1 table is doubled; the E8M0 scale is halved.
                // https://github.com/ggml-org/ggml/blob/master/src/ggml-quants.c
                let values = [
                    0., 1., 2., 3., 4., 6., 8., 12., 0., -1., -2., -3., -4., -6., -8., -12.,
                ];
                let e = b[0] as u32;
                let scale = f32::from_bits(if e < 2 {
                    0x00200000 << e
                } else {
                    (e - 1) << 23
                });
                let q = if j < 16 {
                    b[1 + j] & 15
                } else {
                    b[1 + j - 16] >> 4
                };
                values[q as usize] * scale
            }
            _ => unreachable!(),
        };
        Ok(TensorValue::Float(value))
    }
}

/// Layout of one encoded quantization block.
#[derive(Clone, Copy)]
pub struct Quantization {
    /// Number of values sharing block metadata.
    pub block_len: usize,
    /// Metadata columns preceding the raw codes.
    pub metadata: &'static [&'static str],
}

impl TensorData {
    /// Return the encoded block layout, or `None` for directly stored scalars.
    pub fn quantization(&self) -> Option<Quantization> {
        let metadata: &'static [&'static str] = match self.type_id {
            2 | 6 | 8 | 39 => &["Scale"],
            3 | 7 => &["Scale", "Min"],
            9 => &["Scale", "Sum"],
            23 => &[
                "Scale",
                "Subscale0",
                "Subscale1",
                "Subscale2",
                "Subscale3",
                "Subscale4",
                "Subscale5",
                "Subscale6",
                "Subscale7",
            ],
            _ => return None,
        };
        Some(Quantization {
            block_len: if self.type_id == 23 { 256 } else { 32 },
            metadata,
        })
    }

    /// Read stored block metadata or an unmodified integer code as text.
    /// FP16 metadata is shown as its unsigned 16-bit storage word; MXFP4 scales
    /// are shown as their unsigned E8M0 exponent byte.
    pub fn quantized_field(&self, block: u64, field: usize) -> Result<String> {
        Ok(format!("{:.0}", self.quantized_number(block, field)?))
    }

    /// Read an integer storage word, exponent byte, or raw quantization code.
    pub fn quantized_number(&self, block: u64, field: usize) -> Result<f64> {
        let layout = self
            .quantization()
            .context("Tensor is not block-quantized")?;
        anyhow::ensure!(
            block < self.elements / layout.block_len as u64,
            "Block index out of range"
        );
        anyhow::ensure!(
            field < layout.metadata.len() + layout.block_len,
            "Block field out of range"
        );
        let size = match self.type_id {
            2 => 18,
            3 => 20,
            6 => 22,
            7 => 24,
            8 => 34,
            9 => 36,
            39 => 17,
            23 => 136,
            _ => unreachable!(),
        };
        let start = self.offset + usize::try_from(block)? * size;
        let b = self
            .mapping
            .get(start..start + size)
            .context("Truncated quantization block")?;
        let u16_at = |i| {
            let a = [b[i], b[i + 1]];
            if self.little {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }
        };
        if self.type_id == 23 {
            return Ok(if field == 0 {
                f64::from(u16_at(0))
            } else if field < layout.metadata.len() {
                f64::from(iq4_xs_subscale(b, u16_at(2), field - 1))
            } else {
                let j = field - layout.metadata.len();
                let packed = b[8 + (j / 32) * 16 + j % 16];
                f64::from(if j % 32 < 16 {
                    packed & 15
                } else {
                    packed >> 4
                })
            });
        }
        if field < layout.metadata.len() {
            return Ok(if self.type_id == 39 {
                f64::from(b[0])
            } else {
                f64::from(u16_at(field * 2))
            });
        }
        let j = field - layout.metadata.len();
        if self.type_id == 8 || self.type_id == 9 {
            let offset = if self.type_id == 8 { 2 } else { 4 };
            return Ok(f64::from(b[offset + j] as i8));
        }
        let packed = match self.type_id {
            2 => 2,
            3 => 4,
            6 => 6,
            7 => 8,
            39 => 1,
            _ => unreachable!(),
        };
        let mut q = if j < 16 {
            b[packed + j] & 15
        } else {
            b[packed + j - 16] >> 4
        };
        if self.type_id == 6 || self.type_id == 7 {
            let a: [u8; 4] = b[packed - 4..packed].try_into().unwrap();
            let high = if self.little {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            };
            q |= (((high >> j) & 1) as u8) << 4;
        }
        Ok(f64::from(q))
    }
}

// Unsigned six-bit storage code; decoding subtracts 32 from this value.
fn iq4_xs_subscale(block: &[u8], high: u16, group: usize) -> u8 {
    ((block[4 + group / 2] >> (4 * (group % 2))) & 15) | (((high >> (2 * group)) & 3) as u8) << 4
}
