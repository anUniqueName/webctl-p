//! 视觉识别：在页面截图里做灰度模板匹配，找出和模板图片最像的位置。
//!
//! 算法是两级零均值归一化互相关（ZNCC）：先把截图和模板一起降采样做粗匹配，
//! 再回原图在候选点附近精修。ZNCC 减去了各自均值、除以各自方差，
//! 对整体亮度和对比度变化不敏感；窗口统计量用积分图算，耗时跟模板大小基本无关。
//!
//! 不做缩放不变性：模板必须来自同一缩放比例、同一 devicePixelRatio 下的截图，
//! 最稳的做法是从 `webctl screenshot` 刚截的图里裁。

use anyhow::{Context, Result};

/// 8bit 灰度图，行优先
#[derive(Clone)]
pub struct GrayImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// 一次匹配，(x, y) 是模板左上角在图里的像素坐标
#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub x: f64,
    pub y: f64,
    pub score: f64,
}

/// 解码 PNG 并转成灰度。调色板、16bit、透明通道统一归一到 8bit 灰度。
pub fn decode_png(bytes: &[u8]) -> Result<GrayImage> {
    let mut decoder = png::Decoder::new(bytes);
    // EXPAND 把调色板/低位深展开成 8bit 通道，STRIP_16 把 16bit 砍成 8bit
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("无法解析 PNG 头")?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("无法解码 PNG 像素")?;
    let data = &buf[..info.buffer_size()];
    let (width, height) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb | png::ColorType::Indexed => 3,
        png::ColorType::Rgba => 4,
    };
    let mut gray = vec![0u8; width * height];
    for (i, px) in gray.iter_mut().enumerate() {
        let p = &data[i * channels..i * channels + channels];
        // Rec.601 亮度
        *px = if channels <= 2 {
            p[0]
        } else {
            ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8
        };
    }
    Ok(GrayImage {
        width,
        height,
        data: gray,
    })
}

/// 在 haystack 里找 template，返回 score >= threshold 的匹配，按分数从高到低，最多 max 个。
/// 找不到或模板比图还大时返回空 vec。
pub fn find(haystack: &GrayImage, template: &GrayImage, threshold: f64, max: usize) -> Vec<Match> {
    if max == 0
        || template.width == 0
        || template.height == 0
        || template.width > haystack.width
        || template.height > haystack.height
    {
        return vec![];
    }

    // 粗匹配降采样倍数：让粗模板短边约 8px。倍数越大越快，上限 8 防粗模板退化
    let factor = (template.width.min(template.height) / 8).clamp(1, 8);
    let (coarse_page, coarse_tpl) = if factor > 1 {
        (downsample(haystack, factor), downsample(template, factor))
    } else {
        (haystack.clone(), template.clone())
    };
    if coarse_tpl.width > coarse_page.width || coarse_tpl.height > coarse_page.height {
        return vec![];
    }

    // 粗匹配只负责挑候选，判定交给原图精修。目标左上角和降采样网格对不齐时粗分数会掉：
    // 实测 42x25 的文字按钮（倍数 3）在 9 种对齐偏移下粗分数 0.67–1.0，按原阈值筛会漏掉一大半位置。
    // ponytail: 放宽 0.4 是按这组实测定的；更小、笔画更细的模板若还漏，再加大或改成只取前 N 名
    let coarse_threshold = threshold - 0.4;
    let page_integral = Integral::new(&coarse_page);
    let (tmean, tnorm) = template_stats(&coarse_tpl);
    let mut candidates: Vec<Candidate> = Vec::new();
    for y in 0..=coarse_page.height - coarse_tpl.height {
        for x in 0..=coarse_page.width - coarse_tpl.width {
            let score = zncc(
                &page_integral,
                &coarse_page,
                &coarse_tpl,
                x,
                y,
                tmean,
                tnorm,
            );
            if score >= coarse_threshold {
                candidates.push(Candidate { x, y, score });
            }
        }
    }
    // 大面积相似区域（低阈值时常见）会爆出几十万候选，只留最高的一批再NMS
    if candidates.len() > 50_000 {
        candidates.select_nth_unstable_by(50_000, |a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(50_000);
    }
    let radius = coarse_tpl.width.max(coarse_tpl.height) as f64 / 2.0;
    // 粗阈值放宽后，同尺寸的相似按钮也会进候选，粗分数还可能高过没对齐的真目标；
    // 名额少了（之前 --max 1 只留 4 个）真目标会被挤掉。
    // ponytail: 至少留 32 个；一屏里长得像的元素超过这个数（大表格每行同样的按钮）仍可能漏，到时按模板大小算名额
    let candidates = nms(candidates, radius, max.saturating_mul(4).clamp(32, 64));
    // 找不到匹配是常见路径，别为它白建原图积分图（4K 截图约 132MB）
    if candidates.is_empty() {
        return vec![];
    }
    // 回原图在候选点附近 ±(factor+1) 窗口里精修
    let full_integral = Integral::new(haystack);
    let (tmean, tnorm) = template_stats(template);
    let margin = factor + 1;
    let mut matches: Vec<Candidate> = candidates
        .iter()
        .map(|coarse| {
            let base_x = coarse.x * factor;
            let base_y = coarse.y * factor;
            let mut best = Candidate {
                x: base_x,
                y: base_y,
                score: f64::MIN,
            };
            let y_end = (base_y + margin).min(haystack.height - template.height);
            let x_end = (base_x + margin).min(haystack.width - template.width);
            for y in base_y.saturating_sub(margin)..=y_end {
                for x in base_x.saturating_sub(margin)..=x_end {
                    let score = zncc(&full_integral, haystack, template, x, y, tmean, tnorm);
                    if score > best.score {
                        best = Candidate { x, y, score };
                    }
                }
            }
            best
        })
        .filter(|m| m.score >= threshold)
        .collect();

    let radius = template.width.min(template.height) as f64 / 4.0;
    matches = nms(matches, radius, max);
    matches
        .iter()
        .map(|m| Match {
            x: m.x as f64,
            y: m.y as f64,
            score: m.score,
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Candidate {
    x: usize,
    y: usize,
    score: f64,
}

/// 按分数从高到低做非极大值抑制：距离比 radius 近的候选只留分数最高的
fn nms(candidates: Vec<Candidate>, radius: f64, max: usize) -> Vec<Candidate> {
    let mut sorted = candidates;
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<Candidate> = Vec::new();
    'outer: for candidate in sorted {
        for other in &kept {
            let dx = candidate.x as f64 - other.x as f64;
            let dy = candidate.y as f64 - other.y as f64;
            if dx * dx + dy * dy < radius * radius {
                continue 'outer;
            }
        }
        kept.push(candidate);
        if kept.len() == max {
            break;
        }
    }
    kept
}

fn downsample(image: &GrayImage, factor: usize) -> GrayImage {
    let width = image.width / factor;
    let height = image.height / factor;
    let area = (factor * factor) as u32;
    let mut data = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0u32;
            for dy in 0..factor {
                let row = (y * factor + dy) * image.width;
                for dx in 0..factor {
                    acc += image.data[row + x * factor + dx] as u32;
                }
            }
            data[y * width + x] = (acc / area) as u8;
        }
    }
    GrayImage {
        width,
        height,
        data,
    }
}

/// 模板的均值和去均值后的模长，整个搜索过程不变，先算好
fn template_stats(template: &GrayImage) -> (f64, f64) {
    let n = template.data.len() as f64;
    let mean = template.data.iter().map(|&v| v as f64).sum::<f64>() / n;
    let norm = template
        .data
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        .sqrt();
    (mean, norm)
}

/// 单个位置的零均值归一化互相关，返回 [-1, 1]，1 是完全一致
fn zncc(
    integral: &Integral,
    image: &GrayImage,
    template: &GrayImage,
    x: usize,
    y: usize,
    tmean: f64,
    tnorm: f64,
) -> f64 {
    if tnorm == 0.0 {
        return 0.0;
    }
    let n = (template.width * template.height) as f64;
    let (sum, sumsq) = integral.window(x, y, template.width, template.height);
    let wmean = sum / n;
    let wvar = sumsq - n * wmean * wmean;
    if wvar <= 0.0 {
        return 0.0;
    }
    let mut dot = 0.0;
    for ty in 0..template.height {
        let row = (y + ty) * image.width + x;
        let trow = ty * template.width;
        for tx in 0..template.width {
            dot += image.data[row + tx] as f64 * template.data[trow + tx] as f64;
        }
    }
    ((dot - wmean * n * tmean) / (wvar.sqrt() * tnorm)).clamp(-1.0, 1.0)
}

/// 积分图：任意矩形窗口的像素和、平方和都能 O(1) 算出来
struct Integral {
    stride: usize,
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Integral {
    fn new(image: &GrayImage) -> Self {
        let stride = image.width + 1;
        let mut sum = vec![0.0; stride * (image.height + 1)];
        let mut sq = vec![0.0; stride * (image.height + 1)];
        for y in 0..image.height {
            let mut row_sum = 0.0;
            let mut row_sq = 0.0;
            for x in 0..image.width {
                let v = image.data[y * image.width + x] as f64;
                row_sum += v;
                row_sq += v * v;
                let i = (y + 1) * stride + x + 1;
                sum[i] = sum[i - stride] + row_sum;
                sq[i] = sq[i - stride] + row_sq;
            }
        }
        Integral { stride, sum, sq }
    }

    fn window(&self, x: usize, y: usize, w: usize, h: usize) -> (f64, f64) {
        let a = y * self.stride + x;
        let b = a + w;
        let c = a + h * self.stride;
        let d = c + w;
        (
            self.sum[d] - self.sum[b] - self.sum[c] + self.sum[a],
            self.sq[d] - self.sq[b] - self.sq[c] + self.sq[a],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机图，避免周期性纹理造成自匹配
    fn noise_image(width: usize, height: usize) -> GrayImage {
        let mut data = vec![0u8; width * height];
        let mut state = 0x243F6A8885A308D3u64;
        for v in data.iter_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *v = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8;
        }
        GrayImage {
            width,
            height,
            data,
        }
    }

    fn paste(haystack: &mut GrayImage, template: &GrayImage, x: usize, y: usize) {
        for ty in 0..template.height {
            let row = (y + ty) * haystack.width + x;
            let trow = ty * template.width;
            haystack.data[row..row + template.width]
                .copy_from_slice(&template.data[trow..trow + template.width]);
        }
    }

    #[test]
    fn finds_embedded_template() {
        let mut page = noise_image(160, 120);
        let template = noise_image(16, 12);
        paste(&mut page, &template, 37, 23);
        let matches = find(&page, &template, 0.9, 1);
        assert_eq!(matches.len(), 1, "应找到唯一匹配：{matches:?}");
        let m = matches[0];
        assert!((m.x - 37.0).abs() <= 1.0, "横坐标偏差过大：{m:?}");
        assert!((m.y - 23.0).abs() <= 1.0, "纵坐标偏差过大：{m:?}");
        assert!(m.score > 0.99, "原样嵌入应接近满分：{m:?}");
    }

    #[test]
    fn finds_multiple_instances() {
        let mut page = noise_image(200, 150);
        let template = noise_image(14, 14);
        paste(&mut page, &template, 10, 10);
        paste(&mut page, &template, 150, 100);
        let matches = find(&page, &template, 0.9, 5);
        assert_eq!(matches.len(), 2, "两处都应找到：{matches:?}");
    }

    #[test]
    fn rejects_absent_template() {
        let page = noise_image(160, 120);
        let template = noise_image(16, 16);
        let matches = find(&page, &template, 0.8, 1);
        assert!(matches.is_empty(), "随机模板不该匹配上：{matches:?}");
    }

    #[test]
    fn rejects_constant_template() {
        let mut page = noise_image(120, 90);
        let template = GrayImage {
            width: 10,
            height: 10,
            data: vec![128; 100],
        };
        paste(&mut page, &template, 30, 30);
        let matches = find(&page, &template, 0.5, 1);
        assert!(
            matches.is_empty(),
            "纯色模板没有特征，不该匹配：{matches:?}"
        );
    }

    #[test]
    fn tolerates_brightness_shift() {
        let mut page = noise_image(160, 120);
        let template = noise_image(16, 12);
        // 贴进去的时候整体提亮 40，模拟页面主题和截图时的亮度差
        let brightened: Vec<u8> = template
            .data
            .iter()
            .map(|&v| v.saturating_add(40))
            .collect();
        let shifted = GrayImage {
            width: template.width,
            height: template.height,
            data: brightened,
        };
        paste(&mut page, &shifted, 60, 40);
        let matches = find(&page, &template, 0.8, 1);
        assert_eq!(matches.len(), 1, "亮度平移后仍应匹配：{matches:?}");
        assert!((matches[0].x - 60.0).abs() <= 1.0, "{matches:?}");
    }

    /// 边缘清晰的小按钮：左上角落在降采样网格的哪个偏移上都要找得到
    #[test]
    fn finds_button_at_any_grid_offset() {
        // 42x25（降采样倍数 3）：浅灰底、1px 深色边框、中间几道 2px 宽的笔画
        let (w, h) = (42, 25);
        let mut template = GrayImage {
            width: w,
            height: h,
            data: vec![230; w * h],
        };
        for y in 0..h {
            for x in 0..w {
                let border = x == 0 || y == 0 || x == w - 1 || y == h - 1;
                let stroke = (8..34).contains(&x)
                    && (7..18).contains(&y)
                    && ((x / 2) % 3 == 0 || y == 7 || y == 12 || y == 17);
                if border || stroke {
                    template.data[y * w + x] = 40;
                }
            }
        }
        for oy in 0..3 {
            for ox in 0..3 {
                let mut page = GrayImage {
                    width: 200,
                    height: 120,
                    data: vec![255; 200 * 120],
                };
                paste(&mut page, &template, 60 + ox, 30 + oy);
                let matches = find(&page, &template, 0.8, 1);
                assert_eq!(matches.len(), 1, "偏移 ({ox},{oy}) 没找到：{matches:?}");
                assert_eq!(
                    (matches[0].x, matches[0].y),
                    ((60 + ox) as f64, (30 + oy) as f64)
                );
            }
        }
    }
}
