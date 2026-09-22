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
    Ok(decode_png_rgba(bytes)?.gray)
}

/// 带透明通道的图：灰度 + 可选 alpha。滑块小块用 alpha 抠出形状轮廓。
pub struct Image {
    pub gray: GrayImage,
    pub alpha: Option<Vec<u8>>,
}

/// 解码 PNG/JPEG/WebP 并转成灰度，保留 alpha 通道（有的话）。
/// PNG 走内置解码器，其余格式用 image crate。
pub fn decode_image(bytes: &[u8]) -> Result<Image> {
    const PNG_MAGIC: &[u8] = b"\x89PNG";
    if bytes.starts_with(PNG_MAGIC) {
        return decode_png_rgba(bytes);
    }
    let dynamic = image::load_from_memory(bytes).context("无法解码图片（支持 PNG/JPEG/WebP）")?;
    let rgba = dynamic.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let mut gray = vec![0u8; width * height];
    let mut alpha = vec![0u8; width * height];
    let mut any_transparent = false;
    for (i, px) in rgba.pixels().enumerate() {
        let [r, g, b, a] = px.0;
        gray[i] = ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8;
        alpha[i] = a;
        if a < 255 {
            any_transparent = true;
        }
    }
    Ok(Image {
        gray: GrayImage {
            width,
            height,
            data: gray,
        },
        alpha: any_transparent.then_some(alpha),
    })
}

/// PNG 解码，比 decode_png 多保留 alpha 通道
fn decode_png_rgba(bytes: &[u8]) -> Result<Image> {
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
    let has_alpha = matches!(
        info.color_type,
        png::ColorType::GrayscaleAlpha | png::ColorType::Rgba
    );
    let mut gray = vec![0u8; width * height];
    let mut alpha = vec![0u8; width * height];
    let mut any_transparent = false;
    for (i, px) in gray.iter_mut().enumerate() {
        let p = &data[i * channels..i * channels + channels];
        // Rec.601 亮度
        *px = if channels <= 2 {
            p[0]
        } else {
            ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8
        };
        if has_alpha {
            let a = p[channels - 1];
            alpha[i] = a;
            if a < 255 {
                any_transparent = true;
            }
        }
    }
    Ok(Image {
        gray: GrayImage {
            width,
            height,
            data: gray,
        },
        alpha: (has_alpha && any_transparent).then_some(alpha),
    })
}

/// 一个缺口候选：缺口在图里的包围盒和各项打分依据
#[derive(Debug, Clone)]
pub struct Gap {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub score: f64,
    /// 和滑块形状的 IoU（没给滑块图时为 0）
    pub iou: f64,
    /// 在几个暗度阈值下都稳定出现
    pub stability: usize,
    /// 缺口内部相对周围的平均变暗量
    pub darkness: f64,
}

/// 在滑块验证码背景图里找缺口：缺口是一块被压暗、边缘描了亮边的区域。
/// 做法是算每个像素相对局部均值的变暗量（盒式模糊减原图），在多个阈值下取连通域，
/// 跨阈值稳定出现、暗得明显、形状和滑块块吻合（给了 --piece 时）的候选排前面。
/// 多缺口干扰时全都会返回，按分数从高到低，最多 max 个。
pub fn find_gap(bg: &GrayImage, piece: Option<&Image>, max: usize) -> Vec<Gap> {
    if max == 0 || bg.width < 20 || bg.height < 20 {
        return vec![];
    }
    // 滑块形状：alpha 抠出非透明部分的包围盒
    let shape = piece.and_then(|p| p.alpha.as_ref().and_then(|a| shape_mask(&p.gray, a)));
    let (pw, ph) = shape.as_ref().map(|(w, h, _)| (*w, *h)).unwrap_or((0, 0));
    // 局部均值的窗口要比缺口还大，缺口内部才会整体显得比周围暗：半径取滑块尺寸的一半。
    // 上限跟着滑块走：3 倍图的滑块有 144x156，半径卡在 40 就比缺口还小，一个候选都出不来。
    // 没给滑块图时不知道缺口多大，16、28、40 三个半径各算一遍，结果一起合并：
    // 固定 16（窗口 33x33）比常见的 48x52 缺口还小，缺口内部显不出整体变暗，只检出破碎的边缘，
    // 真缺口会被同一块干扰物的几段边缘挤出前 3 名
    let radii: Vec<usize> = match shape.as_ref() {
        Some((w, h, _)) => vec![((*w).max(*h) / 2).clamp(12, 120)],
        None => vec![16, 28, 40],
    };
    let integral = Integral::new(bg);
    let mut dark = vec![0f32; bg.width * bg.height];
    // 连通域面积上限也跟着滑块走：缺口再怎么也不会有滑块的两倍大。
    // 没给滑块图时沿用固定值
    let max_area = shape.as_ref().map(|(w, h, _)| w * h * 2).unwrap_or(8_000);
    let mut detections: Vec<Detection> = Vec::new();
    for radius in radii {
        // 靠边的像素把窗口整体往里挪，不是截短：截短后窗口变小、统计到的范围不够，
        // 边上缺口的包围盒会被压小（实测 x 差 4、y 差 32）
        let (win_w, win_h) = (
            (2 * radius + 1).min(bg.width),
            (2 * radius + 1).min(bg.height),
        );
        for y in 0..bg.height {
            let y0 = y.saturating_sub(radius).min(bg.height - win_h);
            for x in 0..bg.width {
                let x0 = x.saturating_sub(radius).min(bg.width - win_w);
                let (sum, _) = integral.window(x0, y0, win_w, win_h);
                let mean = sum / (win_w * win_h) as f64;
                dark[y * bg.width + x] = (mean - bg.data[y * bg.width + x] as f64).max(0.0) as f32;
            }
        }
        for th in [10.0f32, 15.0, 20.0, 25.0, 30.0] {
            for comp in components(&dark, bg.width, bg.height, th, max_area) {
                // 缺口尺寸和滑块差不多，太宽太扁的都是背景噪声
                let (min_w, min_h, max_w, max_h) = if shape.is_some() {
                    (
                        pw.saturating_sub(25).max(15),
                        ph.saturating_sub(25).max(15),
                        pw + 25,
                        ph + 25,
                    )
                } else {
                    (20, 20, 130, 130)
                };
                if comp.w < min_w || comp.h < min_h || comp.w > max_w || comp.h > max_h {
                    continue;
                }
                let darkness = comp
                    .pixels
                    .iter()
                    .map(|&i| dark[i as usize] as f64)
                    .sum::<f64>()
                    / comp.pixels.len() as f64;
                let iou = shape
                    .as_ref()
                    .map(|(sw, sh, mask)| shape_iou(&comp, bg.width, *sw, *sh, mask))
                    .unwrap_or(0.0);
                detections.push(Detection {
                    darkness,
                    iou,
                    ..comp
                });
            }
        }
    }
    // 按位置合并不同阈值下的同一处检出：稳定性 +1，iou/暗度取最大
    detections.sort_by(|a, b| {
        b.darkness
            .partial_cmp(&a.darkness)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut merged: Vec<Gap> = Vec::new();
    for d in detections {
        let hit = merged
            .iter_mut()
            .find(|m| d.x.abs_diff(m.x) < 12 && d.y.abs_diff(m.y) < 12);
        match hit {
            Some(m) => {
                // 包围盒取并集：高阈值下的检出偏小，别把完整缺口丢掉
                let (x1, y1) = ((m.x + m.w).max(d.x + d.w), (m.y + m.h).max(d.y + d.h));
                m.x = m.x.min(d.x);
                m.y = m.y.min(d.y);
                m.w = x1 - m.x;
                m.h = y1 - m.y;
                m.stability += 1;
                m.iou = m.iou.max(d.iou);
                m.darkness = m.darkness.max(d.darkness);
            }
            None => merged.push(Gap {
                x: d.x,
                y: d.y,
                w: d.w,
                h: d.h,
                score: 0.0,
                iou: d.iou,
                stability: 1,
                darkness: d.darkness,
            }),
        }
    }
    for m in merged.iter_mut() {
        // darkness 封顶 60：均匀深色背景区域不该靠暗度压过跨阈值稳定和形状吻合；
        // 形状 IoU 权重最高：多缺口干扰时，和滑块形状吻合的才是真缺口
        m.score = m.stability as f64 + m.iou * 4.0 + m.darkness.min(60.0) / 30.0;
    }
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(max);
    merged
}

struct Detection {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    pixels: Vec<u32>,
    darkness: f64,
    iou: f64,
}

/// alpha>10 的包围盒和二值形状掩码；全透明（没有有效像素）时返回 None
fn shape_mask(piece: &GrayImage, alpha: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let (mut x0, mut y0, mut x1, mut y1) = (piece.width, piece.height, 0usize, 0usize);
    let mut any = false;
    for y in 0..piece.height {
        for x in 0..piece.width {
            if alpha[y * piece.width + x] > 10 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
                any = true;
            }
        }
    }
    if !any {
        return None;
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut mask = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            mask[y * w + x] = (alpha[(y0 + y) * piece.width + x0 + x] > 10) as u8;
        }
    }
    Some((w, h, mask))
}

/// 连通域掩码最近邻缩放到滑块形状大小后算 IoU
fn shape_iou(comp: &Detection, img_width: usize, sw: usize, sh: usize, shape: &[u8]) -> f64 {
    // 连通域掩码标进自己的包围盒
    let mut mask = vec![0u8; comp.w * comp.h];
    for &i in &comp.pixels {
        let (px, py) = (
            i as usize % img_width - comp.x,
            i as usize / img_width - comp.y,
        );
        mask[py * comp.w + px] = 1;
    }
    // 最近邻采样到滑块形状大小后算 IoU
    let (mut inter, mut union) = (0usize, 0usize);
    for y in 0..sh {
        for x in 0..sw {
            let c = mask[(y * comp.h / sh) * comp.w + x * comp.w / sw];
            let s = shape[y * sw + x];
            inter += (c & s) as usize;
            union += (c | s) as usize;
        }
    }
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// dark > th 的 4 邻接连通域，面积 300..=max_area 的才保留
fn components(
    dark: &[f32],
    width: usize,
    height: usize,
    th: f32,
    max_area: usize,
) -> Vec<Detection> {
    let mut visited = vec![false; width * height];
    let mut out = Vec::new();
    for start in 0..width * height {
        if visited[start] || dark[start] <= th {
            continue;
        }
        let mut stack = vec![start as u32];
        visited[start] = true;
        let mut pixels: Vec<u32> = Vec::new();
        let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0usize, 0usize);
        let mut too_big = false;
        while let Some(i) = stack.pop() {
            let i = i as usize;
            let (x, y) = (i % width, i / width);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
            if !too_big {
                pixels.push(i as u32);
                if pixels.len() > max_area {
                    too_big = true;
                }
            }
            for next in [
                (x > 0).then(|| i - 1),
                (x + 1 < width).then(|| i + 1),
                (y > 0).then(|| i - width),
                (y + 1 < height).then(|| i + width),
            ]
            .into_iter()
            .flatten()
            {
                if !visited[next] && dark[next] > th {
                    visited[next] = true;
                    stack.push(next as u32);
                }
            }
        }
        if too_big || pixels.len() < 300 {
            continue;
        }
        out.push(Detection {
            x: x0,
            y: y0,
            w: x1 - x0 + 1,
            h: y1 - y0 + 1,
            pixels,
            darkness: 0.0,
            iou: 0.0,
        });
    }
    out
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
    // 倍数为 1（模板短边不到 16px）时不降采样，粗匹配直接在原图上做，省掉两次复制
    let scaled = (factor > 1).then(|| (downsample(haystack, factor), downsample(template, factor)));
    let (coarse_page, coarse_tpl) = match &scaled {
        Some((page, tpl)) => (page, tpl),
        None => (haystack, template),
    };
    if coarse_tpl.width > coarse_page.width || coarse_tpl.height > coarse_page.height {
        return vec![];
    }

    // 粗匹配只负责挑候选，判定交给原图精修，所以不按阈值筛：目标左上角和降采样网格对不齐时
    // 粗分数会掉，掉多少取决于模板的细节有多细——24x16、笔画只有 1px 的模板在 ox、oy 都是奇数的
    // 偏移上一个候选都出不来（之前按 threshold-0.4 筛，64 个偏移漏 16 个）。
    // 改成全算出来、靠下面的 NMS 取各邻域的最高分再截前 N 名。
    // -1.0 只挡掉纯色窗口（zncc 给负无穷），真实分数都在 [-1, 1] 里。
    // 倍数为 1 时粗匹配就是最终判定（没有降采样，也就没有相位问题），直接按 threshold 筛
    let coarse_threshold = if factor == 1 { threshold } else { -1.0 };
    let page_integral = Integral::new(coarse_page);
    let (tmean, tnorm) = template_stats(coarse_tpl);
    let mut candidates: Vec<Candidate> = Vec::new();
    for y in 0..=coarse_page.height - coarse_tpl.height {
        for x in 0..=coarse_page.width - coarse_tpl.width {
            let score = zncc(&page_integral, coarse_page, coarse_tpl, x, y, tmean, tnorm);
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
    // ponytail: 至少留 32 个；一屏里长得像的元素超过这个数（大表格每行同样的按钮）仍可能漏，到时按模板大小算名额。
    // --max 要得比 64 还多时名额跟着 --max 走，不然要的位置会被默默砍掉
    let candidates = nms(
        candidates,
        radius,
        max.saturating_mul(4).clamp(32, max.max(64)),
    );
    // 找不到匹配是常见路径，别为它白建原图积分图（4K 截图约 132MB）
    if candidates.is_empty() {
        return vec![];
    }
    // 粗积分图之后用不到了，先放掉再建原图那张，两张大表不必同时占着内存
    drop(page_integral);
    let mut matches: Vec<Candidate> = if factor == 1 {
        // 粗匹配就是在原图上做的，分数已经是最终分数，按阈值筛一遍即可，不必再扫一遍同样的数据。
        // 候选是各自邻域里的最高分（NMS 半径比精修窗口大），精修也只会得到同一个位置
        candidates
            .into_iter()
            .filter(|m| m.score >= threshold)
            .collect()
    } else {
        // 回原图在候选点附近 ±(factor+1) 窗口里精修
        let full_integral = Integral::new(haystack);
        let (tmean, tnorm) = template_stats(template);
        let margin = factor + 1;
        candidates
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
            .collect()
    };

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
    // 纯色的模板或窗口没有特征，相关性无从谈起：返回负无穷，不管 --threshold 给到多低都不会被当成匹配
    if tnorm == 0.0 {
        return f64::NEG_INFINITY;
    }
    let n = (template.width * template.height) as f64;
    let (sum, sumsq) = integral.window(x, y, template.width, template.height);
    let wmean = sum / n;
    let wvar = sumsq - n * wmean * wmean;
    if wvar <= 0.0 {
        return f64::NEG_INFINITY;
    }
    // 整数乘积按行累加在 u32 里（一行最多 宽×255²，宽 66000 以内不会溢出），再进 u64，
    // 最后一次性转成 f64：逐像素用 f64 累加，大模板上会丢精度
    let mut dot = 0u64;
    for ty in 0..template.height {
        let row = (y + ty) * image.width + x;
        let trow = ty * template.width;
        let mut row_dot = 0u32;
        for tx in 0..template.width {
            row_dot += image.data[row + tx] as u32 * template.data[trow + tx] as u32;
        }
        dot += row_dot as u64;
    }
    ((dot as f64 - wmean * n * tmean) / (wvar.sqrt() * tnorm)).clamp(-1.0, 1.0)
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
        // --threshold 给 0 甚至负数也不能返回位置：vclick 会照着点下去
        for threshold in [0.0, -1.0] {
            let matches = find(&page, &template, threshold, 3);
            assert!(
                matches.is_empty(),
                "阈值 {threshold} 下纯色模板仍不该匹配：{matches:?}"
            );
        }
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
    /// 笔画只有 1px 的小模板：降采样把这些细节平均掉了，粗匹配分数在"模板和页面副本
    /// 落在不同降采样相位"的偏移上会掉一大截（实测 ox、oy 都是奇数的 16 个位置），
    /// 8x8 个偏移都要找得到
    #[test]
    fn finds_fine_detail_template_at_any_grid_offset() {
        // 24x16（降采样倍数 2）：浅色底、1px 深色边框、内部几道 1px 的竖线和横线
        let (w, h) = (24, 16);
        let mut template = GrayImage {
            width: w,
            height: h,
            data: vec![235; w * h],
        };
        for y in 0..h {
            for x in 0..w {
                let border = x == 0 || y == 0 || x == w - 1 || y == h - 1;
                let stroke =
                    (3..21).contains(&x) && (3..13).contains(&y) && (x % 3 == 0 || y % 4 == 0);
                if border || stroke {
                    template.data[y * w + x] = 30;
                }
            }
        }
        for oy in 0..8 {
            for ox in 0..8 {
                let mut page = GrayImage {
                    width: 400,
                    height: 300,
                    data: vec![255; 400 * 300],
                };
                paste(&mut page, &template, 60 + ox, 40 + oy);
                let matches = find(&page, &template, 0.8, 1);
                assert_eq!(matches.len(), 1, "偏移 ({ox},{oy}) 没找到：{matches:?}");
                assert_eq!(
                    (matches[0].x, matches[0].y),
                    ((60 + ox) as f64, (40 + oy) as f64),
                    "偏移 ({ox},{oy}) 位置不对"
                );
            }
        }
    }

    /// 捏一块压暗的拼图状缺口（方块加圆形凸起），验证能找到。
    /// `scale` 是整图放大倍数，用来造 2 倍、3 倍图的验证码
    fn notch_image(width: usize, height: usize, scale: usize) -> (GrayImage, Vec<u8>) {
        // 形状掩码：32x32 方块 + 顶部半径 10 的圆，按 scale 放大
        let (w, h) = (40 * scale, 50 * scale);
        let mut mask = vec![0u8; w * h];
        for y in 10 * scale..h {
            for x in 4 * scale..w - 4 * scale {
                mask[y * w + x] = 1;
            }
        }
        for y in 0..20 * scale {
            for x in 0..w {
                let dx = x as i64 - 20 * scale as i64;
                let dy = y as i64 - 10 * scale as i64;
                if dx * dx + dy * dy <= 100 * (scale * scale) as i64 {
                    mask[y * w + x] = 1;
                }
            }
        }
        // 有结构的底图：低频渐变加小块纹理，接近真实验证码背景；纹理也跟着放大
        let mut page = GrayImage {
            width,
            height,
            data: (0..width * height)
                .map(|i| {
                    let (x, y) = (i % width / scale, i / width / scale);
                    (100 + (x * 3 + y * 7) % 50 + (x / 4 + y / 4) % 2 * 20) as u8
                })
                .collect(),
        };
        // 缺口处恒定压暗 50
        let (nx, ny) = (100 * scale, 40 * scale);
        for y in 0..h {
            for x in 0..w {
                if mask[y * w + x] == 1 {
                    let i = (ny + y) * width + nx + x;
                    page.data[i] = page.data[i].saturating_sub(50);
                }
            }
        }
        (page, mask)
    }

    #[test]
    fn finds_darkened_notch_without_piece() {
        let (page, _mask) = notch_image(220, 140, 1);
        let gaps = find_gap(&page, None, 5);
        // 没有滑块形状可参考时会返回多个候选，缺口（100,40,40x50）要在其中
        assert!(
            gaps.iter()
                .any(|g| g.x.abs_diff(100) <= 12 && g.y <= 90 && g.y + g.h >= 40),
            "候选里应包含缺口位置：{gaps:?}"
        );
    }

    /// 没给 --piece 时也要把真缺口排进前 3：一张 320x180 的图里有一个 48x52 的拼图状缺口，
    /// 外加一个同样大小的方块干扰。固定半径 16（窗口 33x33）比缺口还小，缺口内部显不出整体变暗，
    /// 真缺口会被背景上的零碎检出挤到第 4 名开外
    #[test]
    fn finds_notch_without_piece_among_distractors() {
        let (width, height) = (320usize, 180usize);
        let mut page = GrayImage {
            width,
            height,
            data: (0..width * height)
                .map(|i| {
                    let (x, y) = (i % width, i / width);
                    let grain = (x * 37 + y * 101 + (x * y) % 13) % 25;
                    (100 + (x * 3 + y * 7) % 50 + (x / 4 + y / 4) % 2 * 20 + grain) as u8
                })
                .collect(),
        };
        let darken =
            |page: &mut GrayImage, x0: usize, y0: usize, inside: &dyn Fn(usize, usize) -> bool| {
                for y in 0..52 {
                    for x in 0..48 {
                        if inside(x, y) {
                            let i = (y0 + y) * width + x0 + x;
                            page.data[i] = page.data[i].saturating_sub(50);
                        }
                    }
                }
            };
        // 真缺口：方块 + 顶部圆形凸起（拼图块的常见形状）
        darken(&mut page, 60, 50, &|x, y| {
            let (dx, dy) = (x as i64 - 24, y as i64 - 12);
            (y >= 12 && (5..43).contains(&x)) || dx * dx + dy * dy <= 144
        });
        // 干扰：同样大小的纯方块
        darken(&mut page, 210, 100, &|_, _| true);

        let gaps = find_gap(&page, None, 3);
        assert!(
            gaps.iter()
                .any(|g| g.x.abs_diff(60) <= 12 && g.y <= 62 && g.y + g.h >= 50),
            "前 3 个候选里应有真缺口（60,50）：{gaps:?}"
        );
    }

    #[test]
    fn tolerates_fully_transparent_piece() {
        // 抠坏的全透明滑块图：不应 panic，退化成无形状打分
        let (page, _mask) = notch_image(220, 140, 1);
        let piece = Image {
            gray: GrayImage {
                width: 40,
                height: 50,
                data: vec![128; 40 * 50],
            },
            alpha: Some(vec![0; 40 * 50]),
        };
        let gaps = find_gap(&page, Some(&piece), 5);
        assert!(
            gaps.iter()
                .any(|g| g.x.abs_diff(100) <= 12 && g.y <= 90 && g.y + g.h >= 40),
            "候选里应包含缺口位置：{gaps:?}"
        );
    }

    #[test]
    fn decodes_jpeg() {
        // image crate 编码一张 JPEG 再走 decode_image 解码回来
        let mut img = image::RgbImage::new(16, 12);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x * 16) as u8, (y * 20) as u8, 128]);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .expect("编码 JPEG 失败");
        let decoded = decode_image(buf.get_ref()).expect("JPEG 解码失败");
        assert_eq!((decoded.gray.width, decoded.gray.height), (16, 12));
        assert!(decoded.alpha.is_none(), "JPEG 没有 alpha 通道");
        // 左上角的红色分量应该在 0 附近、右下角接近 240
        assert!(decoded.gray.data[0] < 60, "{:?}", decoded.gray.data[0]);
        let last = decoded.gray.data[16 * 12 - 1];
        assert!(last > 150, "{last}");
    }

    #[test]
    fn finds_darkened_notch_with_piece_shape() {
        let (page, mask) = notch_image(220, 140, 1);
        let piece = Image {
            gray: GrayImage {
                width: 40,
                height: 50,
                data: vec![128; 40 * 50],
            },
            alpha: Some(mask.iter().map(|&v| v * 255).collect()),
        };
        let gaps = find_gap(&page, Some(&piece), 3);
        assert!(!gaps.is_empty(), "应该检出缺口");
        let g = &gaps[0];
        assert!(
            g.x.abs_diff(100) <= 12 && g.y.abs_diff(40) <= 12,
            "位置偏差过大：{g:?}"
        );
        assert!(g.iou > 0.5, "形状应该吻合：{g:?}");
    }

    /// 3 倍图（真实验证码常见）：局部均值半径和连通域面积上限都要跟着滑块尺寸走，
    /// 用固定值时滑块 120x150 的图一个候选都出不来
    #[test]
    fn finds_notch_in_scaled_image() {
        let (page, mask) = notch_image(660, 420, 3);
        let piece = Image {
            gray: GrayImage {
                width: 120,
                height: 150,
                data: vec![128; 120 * 150],
            },
            alpha: Some(mask.iter().map(|&v| v * 255).collect()),
        };
        let gaps = find_gap(&page, Some(&piece), 3);
        assert!(!gaps.is_empty(), "3 倍图上也该检出缺口");
        let g = &gaps[0];
        assert!(
            g.x.abs_diff(300) <= 36 && g.y.abs_diff(120) <= 36,
            "位置偏差过大：{g:?}"
        );
        assert!(g.iou > 0.5, "形状应该吻合：{g:?}");
    }

    #[test]
    fn rejects_plain_noise() {
        let page = noise_image(220, 140);
        let gaps = find_gap(&page, None, 3);
        assert!(gaps.is_empty(), "纯噪声图不该有缺口：{gaps:?}");
    }
}
