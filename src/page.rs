use crate::browser::Browser;
use crate::vision;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SNAPSHOT_JS: &str = include_str!("js/snapshot.js");
const OBSERVE_JS: &str = include_str!("js/observe.js");

pub fn open(
    browser: &mut Browser,
    url: Option<&str>,
    new_tab: bool,
    timeout_ms: u64,
) -> Result<Value> {
    let Some(url) = url else {
        return Ok(json!({"ok": true, "endpoint": browser.state.endpoint}));
    };
    let url = normalize_url(url)?;
    if new_tab {
        // 先建空白页再走同一条导航流程，保证等到的是目标页面的加载，而不是 about:blank
        let target = browser.new_blank()?;
        browser.set_current(target)?;
    }
    let session = browser.attach_current()?;
    drop_queued_load_events(browser, &session);
    let navigation = browser
        .cdp
        .call("Page.navigate", json!({"url": url}), Some(&session))?;
    if let Some(error) = navigation["errorText"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        bail!("打开 {url} 失败：{error}");
    }
    let loaded = wait_loaded(browser, &session, timeout_ms)?;
    let (url, title) = page_info(browser, &session)?;
    let mut output = json!({
        "ok": true,
        "url": url,
        "title": title,
        "targetId": browser.state.current_target,
        "loaded": loaded
    });
    if !mark_blocked(browser, &session, &mut output) && !loaded {
        output["hint"] = json!(
            "没等页面完全加载完就先返回了（多半是图片、广告等资源还在加载）；要找的内容没出现时用 wait --selector 或 --text 等它"
        );
    }
    Ok(output)
}

/// 认出拦截页、错误页，返回类型：cloudflare、perimeterx、akamai、datadome、ebay_interstitial、puzzle、other。
/// 只是如实报告页面是什么，不做任何绕过。验证通过后 cookie 存在会话自己的配置目录里，之后一段时间不会再拦。
///
/// 厂商专用的特征（#px-captcha、captcha-delivery.com、eBay 的 /splashui/ 路径）一条就算；
/// 标题这类通用特征必须再配一条，免得把某个站自己的"Access Denied"权限页当成拦截页：
/// Akamai 要标题是 Access Denied 并且正文有 Reference #，其余标题要正文很短（拦截页、错误页只有一两句话）。
/// Cloudflare 勾选框控件同理：正常页面的表单里也会嵌，要正文很短、token 还没写入才算。
/// 正文只在标题或选择器先命中时才读，免得每次 open、eval 都读整页 innerText。
const CHALLENGE_JS: &str = r#"(() => {
  const title = (document.title || '').trim();
  const has = selector => !!document.querySelector(selector);
  let text = null;
  const body = () => (text ??= document.body?.innerText || '');
  if (has('#challenge-form, #cf-challenge-running')) return 'cloudflare';
  if (/^(just a moment|attention required|checking your browser|请稍候)/i.test(title)) return 'cloudflare';
  // 勾选框控件也可能嵌在正常页面的评论、结账表单里，所以只在正文很短、还没拿到 token 时才算拦截页。
  // [id^="cf-chl-widget"] 和 turnstile 定位用的是同一个选择器：量得到控件的整页验证，这里也要认得出
  if (has('.cf-turnstile, [id^="cf-chl-widget"], iframe[src*="challenges.cloudflare.com"]') && body().length < 5000
      && !document.querySelector('[name="cf-turnstile-response"]')?.value) return 'cloudflare';
  if (has('#px-captcha')) return 'perimeterx';
  if (/^(robot or human|verify your identity)/i.test(title) && (/\/blocked\b/i.test(location.pathname) || /press\s*(&|and)\s*hold/i.test(body()))) return 'perimeterx';
  if (has('iframe[src*="captcha-delivery.com"], script[src*="captcha-delivery.com"]')) return 'datadome';
  if (/\/splashui\/(challenge|captcha)/i.test(location.pathname)) return 'ebay_interstitial';
  if (/^access denied/i.test(title) && /reference\s*#/i.test(body())) return 'akamai';
  // 拼图、滑块：id 或 class 带 captcha 的元素里写着拖动一类的操作说明。
  // 只看这些元素自己的文字，不看整页：很多正常页面都有 class 带 captcha 的 reCAPTCHA 角标。
  // 先去掉没渲染的（display:none 的元素 innerText 返回全部文字，正常页面预先放好的隐藏滑块模板会被误认），再取前 20 个
  const captcha = [...document.querySelectorAll('[id*="captcha" i], [class*="captcha" i]')]
    .filter(el => el.getClientRects().length > 0).slice(0, 20);
  if (captcha.some(el => /drag the (puzzle|slider)|slide to (complete|verify)|拖动|滑动/i.test(el.innerText || ''))) return 'puzzle';
  // ponytail: 5000 字是估的，商品页、搜索页的正文远超这个数；真有带全站页眉页脚的错误页漏报，再调大
  if (/^(robot or human|access denied|pardon our interruption|security measure|verify your identity|challenge validation|security check|error page)/i.test(title) && body().length < 5000) return 'other';
  return null;
})()"#;

fn detect_challenge(browser: &mut Browser, session: &str) -> Result<Option<String>> {
    let found = eval_value(browser, session, CHALLENGE_JS)?;
    Ok(found.as_str().map(str::to_owned))
}

/// open、back、reload、eval 共用：认出拦截页时往输出里加 blocked 和按类型写的提示，返回是否认出。
/// 识别只是附加信息，出错（比如页面正在跳转）时当作没认出，不影响命令本身的结果
fn mark_blocked(browser: &mut Browser, session: &str, output: &mut Value) -> bool {
    let Ok(Some(kind)) = detect_challenge(browser, session) else {
        return false;
    };
    output["hint"] = json!(challenge_hint(&kind));
    output["blocked"] = json!(kind);
    true
}

/// 按拦截类型写下一步。要不要处理、怎么处理由调用方按自己的规定决定，webctl 不替它定
fn challenge_hint(kind: &str) -> &'static str {
    match kind {
        "cloudflare" => {
            "当前页面是 Cloudflare 人机验证页，不是目标内容。页面上有勾选框时可以用 `webctl turnstile` 点一下；点不过去或没有勾选框，按调用方自己的规定交人工或记为拦截。交人工时先 `webctl front` 切到前台，再用 `webctl wait --text <目标页上的文字> --timeout 300000` 等验证完成"
        }
        "perimeterx" => {
            "当前页面是 PerimeterX 长按验证页，不是目标内容。需要按住才能过：调用方按自己的规定决定是否用 `clickat X Y --hold 毫秒`，或交人工（先 `webctl front` 切到前台）；webctl 不会自动处理"
        }
        "puzzle" => {
            "当前页面是拼图或滑块验证页，不是目标内容。需要拖动才能过：调用方按自己的规定决定是否用 `drag X1 Y1 X2 Y2`，或交人工（先 `webctl front` 切到前台）；webctl 不会自动处理"
        }
        "akamai" => {
            "站点拒绝访问（Akamai 拦截页），页面上没有目标内容：记为拦截，不要当成 0 结果或拿这个页面的内容当数据"
        }
        "datadome" => {
            "DataDome 验证页，页面上没有目标内容，不要当成 0 结果。有的是设备检查页，几秒后自己跳回原页（地址带 dd_referrer）：先 `webctl wait --gone 'iframe[src*=\"captcha-delivery.com\"]' --timeout 10000` 再读；还在就是站点拒绝访问，记为拦截"
        }
        "ebay_interstitial" => {
            "当前页面是 eBay 的验证过渡页，不是目标内容。它有时过几秒会自己跳回目标页：用 `webctl wait --url <目标地址片段>` 等，或稍后重新 open；一直停在这里就按调用方自己的规定交人工或记为拦截"
        }
        _ => {
            "页面标题像拦截页或错误页，不是目标内容，不要当成 0 结果；先 screenshot 看一眼，再按调用方自己的规定重试、交人工或记为拦截"
        }
    }
}

/// 量出验证控件在页面上的位置，不进控件内部。
/// Turnstile 常把 iframe 放在容器的封闭 shadow root 里，JS 完全够不着；但容器本身就在控件的位置上，
/// 拿容器的矩形算坐标一样点得到——鼠标事件发的是页面坐标，由浏览器自己路由进去。
const TURNSTILE_JS: &str = r#"(() => {
  const box = el => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 ? {x: r.left, y: r.top, w: r.width, h: r.height} : null;
  };
  // 看全部命中，取第一个量得出尺寸的：第一个命中常常是 0×0 的元素
  const first = (selector, measure) => {
    for (const el of document.querySelectorAll(selector)) {
      const found = measure(el);
      if (found) return found;
    }
    return null;
  };
  const framed = first('iframe[src*="challenges.cloudflare.com"]', box);
  if (framed) return Object.assign({kind: 'widget_visible', from: 'iframe'}, framed);
  // 整页验证的主文档里常常只有 0×0 的隐藏字段 cf-chl-widget-xxx_response，量它的父元素：
  // 父元素一般就是挂封闭 shadow root 的容器，勾选框就在里面。
  // 只对隐藏字段这样做：容器自己 0×0 是控件还没渲染出来，它的父元素可能是整个表单，点上去会误点别的东西
  const hosted = first('.cf-turnstile, [id^="cf-chl-widget"]', el =>
    box(el) || (el.localName === 'input' && el.parentElement ? box(el.parentElement) : null));
  if (hosted) return Object.assign({kind: 'widget_visible', from: 'container'}, hosted);
  if (document.querySelector('.cf-turnstile, [id^="cf-chl-widget"], #challenge-form, #cf-challenge-running')) {
    return {kind: 'widget_hidden'};
  }
  return {kind: 'none'};
})()"#;

/// 点一下验证控件上勾选框所在的位置——和真人点的是同一个动作，用的也是同一套真实鼠标事件。
/// 不改指纹、不改环境、不碰控件内部：只量出控件在页面上的位置，把鼠标事件发到那个坐标。
///
/// 注意这里只判断得出控件**可不可见**，判断不出它**能不能点**：控件内部在封闭 shadow root 或
/// 跨域 iframe 里，谁也进不去。所以可见就点一下，点完如实报过没过，不替页面下结论。
/// 控件不可见时什么都不做，如实报出。`open` 的行为不变，仍然只报告不处理；要点必须显式敲这条命令。
pub fn turnstile(browser: &mut Browser, timeout_ms: u64) -> Result<Value> {
    let session = browser.attach_current()?;
    // 后台标签页不渲染，控件量不出尺寸，会误报 widget_hidden；和 screenshot 一样先切到前台
    browser
        .cdp
        .call("Page.bringToFront", json!({}), Some(&session))?;
    // 控件常常是先插容器、隔一会儿才撑开，太早量会误判成不可见。
    // 至少量 3 秒；--timeout 给得长就多量一会儿，最多 10 秒
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(3_000, 10_000));
    let mut widget = eval_value(browser, &session, TURNSTILE_JS)?;
    while widget["kind"] != "widget_visible" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
        widget = eval_value(browser, &session, TURNSTILE_JS)?;
    }

    let kind = widget["kind"].as_str().unwrap_or("none").to_owned();
    if kind != "widget_visible" {
        let (url, title) = page_info(browser, &session)?;
        let mut output =
            json!({"ok": true, "challenge": kind, "clicked": false, "url": url, "title": title});
        if output["challenge"] == "widget_hidden" {
            output["hint"] = json!(
                "没量到可见的验证控件，没有能点的位置。先 screenshot 看页面：看得到勾选框时，调用方按自己的规定决定是否用 clickat 点它；看不到就是非交互式验证，放不放行取决于浏览器环境和出口 IP，按调用方的规定交人工或记为拦截"
            );
        }
        return Ok(output);
    }

    if turnstile_solved(browser, &session)? {
        let (url, title) = page_info(browser, &session)?;
        return Ok(json!({
            "ok": true,
            "challenge": "widget_visible",
            "measured_from": widget["from"],
            "clicked": false,
            "passed": true,
            "note": "控件已经是通过状态，没有去点",
            "url": url,
            "title": title
        }));
    }

    let number = |key: &str| -> Result<f64> {
        widget[key]
            .as_f64()
            .ok_or_else(|| anyhow!("质询控件缺少 {key}"))
    };
    // ponytail: 勾选框固定在控件左侧约 30px、垂直居中处。Cloudflare 换版式就得改这个数；
    // 改了最坏也只是点在控件的空白处，不会误点到页面上别的东西
    let x = number("x")? + (number("w")? / 2.0).min(30.0);
    let y = number("y")? + number("h")? / 2.0;
    mouse(browser, &session, "mouseMoved", x, y, "none", 0, 0)?;
    thread::sleep(Duration::from_millis(30));
    click_buttons(browser, &session, x, y, false, false, None)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut passed = false;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(500));
        // 过了的整页验证会跳走，跳转途中执行脚本可能报错，当作还没过、继续等
        if turnstile_solved(browser, &session).unwrap_or(false) {
            passed = true;
            break;
        }
    }
    let (url, title) = page_info(browser, &session)?;
    let mut output = json!({
        "ok": true,
        "challenge": "widget_visible",
        "measured_from": widget["from"],
        "clicked": true,
        "passed": passed,
        "point": {"x": x, "y": y},
        "url": url,
        "title": title
    });
    if !passed {
        output["hint"] = json!(
            "点过了但验证还在，说明这个控件不是点一下就能过的：别再点，按调用方自己的规定交人工（先 `webctl front` 切到前台）或记为拦截"
        );
    }
    Ok(output)
}

/// 两种页面两种通过信号，缺一不可：
/// 内嵌在表单里的控件过了页面不动，但会把 token 写进 cf-turnstile-response 字段；
/// 拦截页过了会跳走，页面上的验证标记随之消失。只看后者会把内嵌控件一律误判成没过。
///
/// "标记消失"要和定位用同一套条件：TURNSTILE_JS 一个控件都量不到，detect_challenge 也认不出拦截页。
/// 只看 detect_challenge 的话，标题正常、控件在封闭 shadow root 里的页面，还没点就会报通过。
/// 文档还在解析时不算"标记消失"：验证页没通过、自己重新加载换一道题时，新文档刚提交，
/// 标题和控件都还没解析出来，两项都会判成没有验证
fn turnstile_solved(browser: &mut Browser, session: &str) -> Result<bool> {
    let state = eval_value(
        browser,
        session,
        r#"(() => {
          const field = document.querySelector('[name="cf-turnstile-response"]');
          return {token: !!(field && field.value), parsing: document.readyState === 'loading'};
        })()"#,
    )?;
    if state["token"] == true {
        return Ok(true);
    }
    if state["parsing"] == true {
        return Ok(false);
    }
    Ok(
        eval_value(browser, session, TURNSTILE_JS)?["kind"] == "none"
            && detect_challenge(browser, session)?.is_none(),
    )
}

pub fn snapshot(browser: &mut Browser, target: Option<&str>, max: usize) -> Result<String> {
    let session = browser.attach_current()?;
    let expression = format!("({SNAPSHOT_JS})({})", json!({"target": target, "max": max}));
    value_string(eval_value(browser, &session, &expression)?)
}

pub fn text(browser: &mut Browser, target: Option<&str>, max: usize) -> Result<String> {
    let session = browser.attach_current()?;
    // 带目标时和 click、fill 走同一套定位（选择器命中多个时先取可见的）
    let body = r"return (el.innerText || '').replace(/\n{3,}/g, '\n\n');";
    let expression = match target {
        Some(target) => target_expression(target, body),
        None => {
            format!("(() => {{ const el = document.body || document.documentElement; {body} }})()")
        }
    };
    let output = value_string(eval_value(browser, &session, &expression)?)?;
    Ok(truncate(&output, max))
}

pub fn click(
    browser: &mut Browser,
    target: &str,
    force: bool,
    right: bool,
    double: bool,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    let session = browser.attach_current()?;
    // 先定位（会滚动页面）再开始记录变化，免得把滚动触发的懒加载内容算成点击结果
    let (x, y) = locate(browser, &session, target, force)?;
    let before = begin_observe(browser, &session)?;
    mouse(browser, &session, "mouseMoved", x, y, "none", 0, 0)?;
    thread::sleep(Duration::from_millis(30));
    click_buttons(browser, &session, x, y, right, double, None)?;
    finish_observe(browser, &session, before, settle_ms, timeout_ms)
}

pub fn hover(
    browser: &mut Browser,
    target: &str,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    let session = browser.attach_current()?;
    let (x, y) = locate(browser, &session, target, false)?;
    let before = begin_observe(browser, &session)?;
    mouse(browser, &session, "mouseMoved", x, y, "none", 0, 0)?;
    finish_observe(browser, &session, before, settle_ms, timeout_ms)
}

/// 视觉识别：在当前页面截图里找模板图片，返回匹配中心的视口 CSS 坐标。
/// 模板应当从 `webctl screenshot` 截的图里裁出来——匹配不做缩放不变性，
/// 页面缩放或 devicePixelRatio 变了模板就对不上。
pub fn find_image(browser: &mut Browser, path: &Path, threshold: f64, max: usize) -> Result<Value> {
    if !(-1.0..=1.0).contains(&threshold) {
        bail!("--threshold 必须在 -1 到 1 之间（常用 0.7–0.95），收到 {threshold}");
    }
    let session = browser.attach_current()?;
    browser
        .cdp
        .call("Page.bringToFront", json!({}), Some(&session))?;
    let (page, scale_x, scale_y) = capture_gray(browser, &session)?;
    let template_bytes =
        fs::read(path).with_context(|| format!("无法读取模板图片 {}", path.display()))?;
    let template = vision::decode_image(&template_bytes)
        .with_context(|| format!("模板图片 {} 无法解码（支持 PNG/JPEG/WebP）", path.display()))?
        .gray;
    let found = vision::find(&page, &template, threshold, max);
    let matches: Vec<Value> = found
        .iter()
        .map(|m| {
            // 截图是设备像素，鼠标事件用的是 CSS 像素，按视口宽高的比例换算回去
            json!({
                "x": (m.x + template.width as f64 / 2.0) / scale_x,
                "y": (m.y + template.height as f64 / 2.0) / scale_y,
                "w": template.width as f64 / scale_x,
                "h": template.height as f64 / scale_y,
                "score": (m.score * 1000.0).round() / 1000.0
            })
        })
        .collect();
    let mut output = json!({
        "ok": true,
        "image": path.display().to_string(),
        "threshold": threshold,
        "matches": matches
    });
    if found.is_empty() {
        output["hint"] =
            json!("没有找到匹配：确认模板是从当前页面同一缩放比例下截的图，或降低 --threshold");
    }
    Ok(output)
}

/// 纯本地的缺口识别：不需要浏览器。背景图、滑块图都是文件，支持 PNG/JPEG/WebP。
pub fn gap_files(bg_path: &Path, piece_path: Option<&Path>, max: usize) -> Result<Value> {
    let bg_bytes =
        fs::read(bg_path).with_context(|| format!("无法读取背景图 {}", bg_path.display()))?;
    let bg = vision::decode_image(&bg_bytes).with_context(|| {
        format!(
            "背景图 {} 无法解码（支持 PNG/JPEG/WebP）",
            bg_path.display()
        )
    })?;
    let piece = match piece_path {
        Some(p) => {
            let bytes = fs::read(p).with_context(|| format!("无法读取滑块图 {}", p.display()))?;
            Some(vision::decode_image(&bytes).with_context(|| {
                format!("滑块图 {} 无法解码（支持 PNG/JPEG/WebP）", p.display())
            })?)
        }
        None => None,
    };
    let gaps = vision::find_gap(&bg.gray, piece.as_ref(), max);
    let candidates: Vec<Value> = gaps
        .iter()
        .map(|g| {
            json!({
                "x": g.x,
                "y": g.y,
                "w": g.w,
                "h": g.h,
                "cx": g.x + g.w / 2,
                "cy": g.y + g.h / 2,
                "score": (g.score * 100.0).round() / 100.0,
                "iou": (g.iou * 1000.0).round() / 1000.0,
                "stability": g.stability,
                "darkness": (g.darkness * 10.0).round() / 10.0
            })
        })
        .collect();
    let mut output = json!({
        "ok": true,
        "bg": bg_path.display().to_string(),
        "piece": piece_path.map(|p| p.display().to_string()),
        "width": bg.gray.width,
        "height": bg.gray.height,
        "candidates": candidates
    });
    if piece.is_none() {
        output["note"] = json!(
            "没给 --piece：只按暗度和固定尺寸范围猜，候选很可能不是真缺口；尽量带上滑块图（最好是带 alpha 的 PNG/WebP）"
        );
    } else if piece.as_ref().is_some_and(|p| p.alpha.is_none()) {
        output["note"] = json!(
            "滑块图没有透明通道，形状打分（iou）未生效；用带 alpha 的 PNG/WebP 滑块图效果最好"
        );
    }
    if gaps.is_empty() {
        output["hint"] = json!(
            "没有找到缺口：确认背景图是页面上实际显示的那张（不是打乱的原始切片）；给了 --piece 时滑块图要和背景图同一比例"
        );
    }
    Ok(output)
}

/// 以拟人轨迹把鼠标移到视口坐标 (x, y)，只移动不点击。
pub fn move_to(browser: &mut Browser, x: f64, y: f64) -> Result<Value> {
    if !x.is_finite() || !y.is_finite() {
        bail!("坐标必须是有限数值，收到 ({x}, {y})");
    }
    let session = browser.attach_current()?;
    human_move(browser, &session, x, y, false, None)?;
    Ok(json!({"ok": true, "x": x, "y": y}))
}

/// 以拟人轨迹移动鼠标并点击视口坐标 (x, y)。
/// 和 `click` 的区别是不经过 DOM 定位：靠坐标点，配合 `find` 处理 canvas、
/// 封闭 shadow root 等 JS 够不着的目标。`hold` 给了毫秒数时按下后保持这么久再松开（长按）。
#[allow(clippy::too_many_arguments)]
pub fn click_at(
    browser: &mut Browser,
    x: f64,
    y: f64,
    right: bool,
    double: bool,
    hold: Option<u64>,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    if !x.is_finite() || !y.is_finite() {
        bail!("坐标必须是有限数值，收到 ({x}, {y})");
    }
    let session = browser.attach_current()?;
    let before = begin_observe(browser, &session)?;
    human_move(browser, &session, x, y, false, None)?;
    click_buttons(browser, &session, x, y, right, double, hold)?;
    finish_observe(browser, &session, before, settle_ms, timeout_ms)
}

/// 按住左键从 `from` 拖到 `to`：拟人轨迹移到起点，按下，按住左键沿拟人轨迹移到终点，松开。
/// 移动途中的事件带 buttons=1，页面看到的是按着左键在移动，自定义滑块、拖拽排序、地图平移靠的就是这个。
/// 只是通用输入操作，不看页面内容，拖到哪由调用方给坐标。
pub fn drag(
    browser: &mut Browser,
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    if ![from.0, from.1, to.0, to.1].iter().all(|v| v.is_finite()) {
        bail!("坐标必须是有限数值，收到 {from:?} → {to:?}");
    }
    let session = browser.attach_current()?;
    let before = begin_observe(browser, &session)?;
    human_move(browser, &session, from.0, from.1, false, None)?;
    mouse(
        browser,
        &session,
        "mousePressed",
        from.0,
        from.1,
        "left",
        1,
        1,
    )?;
    // 人按下之后会停一下才开始拖
    thread::sleep(Duration::from_millis(100));
    let moved = human_move(browser, &session, to.0, to.1, true, Some(duration_ms));
    thread::sleep(Duration::from_millis(50));
    // 移动中途出错也先松开左键，免得页面一直以为左键按着
    mouse(browser, &session, "mouseReleased", to.0, to.1, "left", 0, 1)?;
    moved?;
    finish_observe(browser, &session, before, settle_ms, timeout_ms)
}

/// 视觉识别 + 点击：在截图里找模板图片，以拟人轨迹移过去点击。
/// 找不到匹配时返回 ok:false，不盲点。
pub fn vclick(
    browser: &mut Browser,
    path: &Path,
    threshold: f64,
    right: bool,
    double: bool,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    if !(-1.0..=1.0).contains(&threshold) {
        bail!("--threshold 必须在 -1 到 1 之间（常用 0.7–0.95），收到 {threshold}");
    }
    let session = browser.attach_current()?;
    browser
        .cdp
        .call("Page.bringToFront", json!({}), Some(&session))?;
    let (page, scale_x, scale_y) = capture_gray(browser, &session)?;
    let template_bytes =
        fs::read(path).with_context(|| format!("无法读取模板图片 {}", path.display()))?;
    let template = vision::decode_image(&template_bytes)
        .with_context(|| format!("模板图片 {} 无法解码（支持 PNG/JPEG/WebP）", path.display()))?
        .gray;
    let found = vision::find(&page, &template, threshold, 1);
    let Some(best) = found.first() else {
        return Ok(json!({
            "ok": false,
            "error": format!("在页面上没找到与 {} 匹配的图像", path.display()),
            "hint": "确认模板是从当前页面同一缩放比例下截的图；页面可能还没渲染出来，可先 wait 再试；或降低 --threshold"
        }));
    };
    let x = (best.x + template.width as f64 / 2.0) / scale_x;
    let y = (best.y + template.height as f64 / 2.0) / scale_y;
    let before = begin_observe(browser, &session)?;
    human_move(browser, &session, x, y, false, None)?;
    click_buttons(browser, &session, x, y, right, double, None)?;
    let mut output = finish_observe(browser, &session, before, settle_ms, timeout_ms)?;
    output["match"] = json!({
        "x": x,
        "y": y,
        "score": (best.score * 1000.0).round() / 1000.0
    });
    Ok(output)
}

/// 截当前视口并转成灰度图，附带设备像素到 CSS 像素的换算比例
fn capture_gray(browser: &mut Browser, session: &str) -> Result<(vision::GrayImage, f64, f64)> {
    let result = browser.cdp.call(
        "Page.captureScreenshot",
        json!({"format": "png"}),
        Some(session),
    )?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        result["data"]
            .as_str()
            .ok_or_else(|| anyhow!("Chrome 未返回截图数据"))?,
    )?;
    let page = vision::decode_png(&bytes)?;
    let viewport = eval_value(browser, session, "({w: innerWidth, h: innerHeight})")?;
    let css_w = viewport["w"]
        .as_f64()
        .ok_or_else(|| anyhow!("取不到视口宽度"))?;
    let css_h = viewport["h"]
        .as_f64()
        .ok_or_else(|| anyhow!("取不到视口高度"))?;
    // is_finite 已排除 NaN，<= 0.0 不会漏掉它
    if css_w <= 0.0 || css_h <= 0.0 || !css_w.is_finite() || !css_h.is_finite() {
        bail!("视口尺寸异常（{css_w}x{css_h}），无法换算坐标");
    }
    let scale_x = page.width as f64 / css_w;
    let scale_y = page.height as f64 / css_h;
    Ok((page, scale_x, scale_y))
}

const FILL_CONTROL: &str = "if (!el.matches('input,textarea,select,[contenteditable]') && el.closest('label')?.control) el = el.closest('label').control;";

/// 当前真正有焦点的元素。目标在同源 iframe、开放 shadow root 里时，
/// 顶层文档的 activeElement 是 iframe 或宿主元素，要顺着 contentDocument / shadowRoot 再往里找一层
const ACTIVE_ELEMENT_JS: &str = r#"(() => {
          let el = document.activeElement;
          while (el) {
            let inner = el.shadowRoot ? el.shadowRoot.activeElement : null;
            if (!inner && el.tagName === 'IFRAME') try { inner = el.contentDocument ? el.contentDocument.activeElement : null; } catch (_) {}
            if (!inner) break;
            el = inner;
          }
          return el;
        })()"#;

pub fn fill(
    browser: &mut Browser,
    target: &str,
    text: &str,
    append: bool,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    let session = browser.attach_current()?;
    let before = begin_observe(browser, &session)?;
    // text= 常找到输入框的标签文字，换成标签关联的输入框。
    // 焦点没落到目标上时接着输入会打进之前有焦点的那个框，所以直接报错，并写明原因：
    // 焦点被页面转到了别的元素（点搜索框弹出另一个输入框）、目标不可见、已禁用，或者它本来就不是输入框
    let focus = target_expression(
        target,
        &format!(
            r#"{FILL_CONTROL} const rootNode = el.getRootNode(), before = rootNode.activeElement; el.focus(); const active = rootNode.activeElement;
            if (!active || (active !== el && !el.contains(active))) {{
              if (active && active !== before && active !== rootNode.body) throw new Error(`焦点被页面转到了 ${{describe(active)}}，不在目标 ${{describe(el)}} 上；改 fill 那个元素（先 snapshot 拿它的编号）`);
              if (!visible(el)) throw new Error(`目标 ${{describe(el)}} 不可见，拿不到输入焦点；用 snapshot 找到可见的输入框编号再 fill`);
              if (el.disabled) throw new Error(`目标 ${{describe(el)}} 已禁用，不能输入`);
              throw new Error(`目标 ${{describe(el)}} 拿不到输入焦点，不是输入框；用 snapshot 找到输入框的编号再 fill`);
            }}
            {}"#,
            if append {
                "try { el.setSelectionRange(el.value.length, el.value.length); } catch (_) { const r=document.createRange(); r.selectNodeContents(el); r.collapse(false); const s=getSelection(); s.removeAllRanges(); s.addRange(r); } return el.matches('input[type=password]');"
            } else {
                "return el.matches('input[type=password]');"
            }
        ),
    );
    // 目标是密码框时在输出里标一个 masked，日志会据此把 argv 里的那段文字换成 ***。
    // stdout 照旧输出真实的 value，只多这一个标记
    let masked = eval_value(browser, &session, &focus)? == true;
    if !append {
        select_all(browser, &session)?;
    }
    if !text.is_empty() {
        browser
            .cdp
            .call("Input.insertText", json!({"text": text}), Some(&session))?;
    } else if !append {
        // 清空：全选之后发一次 Backspace 删掉选中的内容。
        // --append 没有选中内容，这一下会把原有内容的最后一个字删掉，所以不发
        dispatch_key(
            browser,
            &session,
            "rawKeyDown",
            "Backspace",
            "Backspace",
            8,
            0,
            None,
            None,
        )?;
        dispatch_key(
            browser,
            &session,
            "keyUp",
            "Backspace",
            "Backspace",
            8,
            0,
            None,
            None,
        )?;
    }
    // 从当前焦点读回填好的值，不重新定位目标：输入本身可能让目标失效
    // （text= 找的是随内容变化的文字、页面重新渲染丢掉 data-webctl-ref），
    // 那时重新定位会报错，看起来像没填进去，重试一次又会填两遍。
    // 焦点由上面那步保证落在目标或它的子元素上。读不回来也不算失败，输出里 value 为 null
    let value = eval_value(
        browser,
        &session,
        &format!(
            "(el => !el ? null : (el.isContentEditable ? el.innerText : el.value))({ACTIVE_ELEMENT_JS})"
        ),
    )
    .unwrap_or(Value::Null);
    let mut output = finish_observe(browser, &session, before, settle_ms, timeout_ms)?;
    output["value"] = value;
    if masked {
        output["masked"] = json!(true);
    }
    Ok(output)
}

pub fn type_text(
    browser: &mut Browser,
    text: &str,
    delay_ms: u64,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    let session = browser.attach_current()?;
    // 焦点在密码框上时标一个 masked，日志会据此把 argv 里的那段文字换成 ***
    let masked = eval_value(
        browser,
        &session,
        &format!("(el => !!el && el.matches('input[type=password]'))({ACTIVE_ELEMENT_JS})"),
    )
    .is_ok_and(|focused| focused == true);
    let before = begin_observe(browser, &session)?;
    for character in text.chars() {
        // 标点的虚拟键码不等于 ASCII 码（'.' 的 46 是 Delete 键），只对字母、数字、空格发按键事件
        if character.is_ascii_alphanumeric() || character == ' ' {
            let key = character.to_string();
            let code = if character.is_ascii_alphabetic() {
                format!("Key{}", character.to_ascii_uppercase())
            } else if character.is_ascii_digit() {
                format!("Digit{character}")
            } else {
                "Space".to_owned()
            };
            let vk = character.to_ascii_uppercase() as u32;
            dispatch_key(
                browser,
                &session,
                "keyDown",
                &key,
                &code,
                vk,
                0,
                Some(&key),
                None,
            )?;
            dispatch_key(browser, &session, "keyUp", &key, &code, vk, 0, None, None)?;
        } else {
            browser.cdp.call(
                "Input.insertText",
                json!({"text": character.to_string()}),
                Some(&session),
            )?;
        }
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    let mut output = finish_observe(browser, &session, before, settle_ms, timeout_ms)?;
    if masked {
        output["masked"] = json!(true);
    }
    Ok(output)
}

pub fn press(browser: &mut Browser, key: &str, settle_ms: u64, timeout_ms: u64) -> Result<Value> {
    let session = browser.attach_current()?;
    let before = begin_observe(browser, &session)?;
    let spec = parse_key(key)?;
    send_key(browser, &session, &spec)?;
    finish_observe(browser, &session, before, settle_ms, timeout_ms)
}

pub fn select(
    browser: &mut Browser,
    target: &str,
    choice: &str,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    let session = browser.attach_current()?;
    let before = begin_observe(browser, &session)?;
    let body = format!(
        r#"if (el.tagName !== 'SELECT') throw new Error('目标不是原生 select；请先 click 展开，再 snapshot 找到选项后点击');
        const choice = {};
        const option = [...el.options].find(o => o.value === choice) || [...el.options].find(o => o.text.trim() === choice);
        if (!option) throw new Error(`找不到选项：${{choice}}`);
        el.value = option.value; el.dispatchEvent(new Event('input', {{bubbles:true}})); el.dispatchEvent(new Event('change', {{bubbles:true}})); return el.value;"#,
        json!(choice)
    );
    let value = eval_value(browser, &session, &target_expression(target, &body))?;
    let mut output = finish_observe(browser, &session, before, settle_ms, timeout_ms)?;
    output["value"] = value;
    Ok(output)
}

pub fn upload(browser: &mut Browser, target: &str, files: &[PathBuf]) -> Result<Value> {
    if files.is_empty() {
        bail!("至少需要一个文件");
    }
    let files = files
        .iter()
        .map(|path| {
            if !path.is_file() {
                bail!("文件不存在：{}", path.display());
            }
            // 不用 canonicalize：Windows 上它返回 \\?\ 开头的路径
            Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let session = browser.attach_current()?;
    let expression = target_expression(
        target,
        "if (!el.matches('input[type=file]')) throw new Error('目标不是 input[type=file]'); return el;",
    );
    let result = eval_raw(browser, &session, &expression, false)?;
    let object_id = result["result"]["objectId"]
        .as_str()
        .ok_or_else(|| anyhow!("无法取得文件输入框 objectId"))?;
    browser.cdp.call(
        "DOM.setFileInputFiles",
        json!({"files": files, "objectId": object_id}),
        Some(&session),
    )?;
    Ok(json!({"ok": true, "files": files}))
}

pub fn scroll(
    browser: &mut Browser,
    direction: &str,
    pixels: Option<i64>,
    target: Option<&str>,
) -> Result<Value> {
    let session = browser.attach_current()?;
    match direction {
        "up" | "down" => {
            let (x, y) = if let Some(target) = target {
                locate(browser, &session, target, true)?
            } else {
                let value = eval_value(
                    browser,
                    &session,
                    "({x: innerWidth / 2, y: innerHeight / 2})",
                )?;
                (
                    value["x"].as_f64().unwrap_or(0.0),
                    value["y"].as_f64().unwrap_or(0.0),
                )
            };
            let amount = pixels.unwrap_or(600).abs() * if direction == "up" { -1 } else { 1 };
            browser.cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": amount}),
                Some(&session),
            )?;
        }
        "top" | "bottom" => {
            let value = if direction == "top" { 0 } else { i64::MAX };
            if let Some(target) = target {
                eval_value(
                    browser,
                    &session,
                    &target_expression(target, &format!("el.scrollTo(0, {value}); return true;")),
                )?;
            } else {
                eval_value(browser, &session, &format!("scrollTo(0, {value}); true"))?;
            }
        }
        _ => bail!("滚动方向必须是 up、down、top 或 bottom"),
    }
    thread::sleep(Duration::from_millis(300));
    let y = eval_value(browser, &session, "Math.round(scrollY)")?;
    Ok(json!({"ok": true, "scrollY": y}))
}

pub fn evaluate(browser: &mut Browser, script: &str, max: usize) -> Result<Value> {
    let session = browser.attach_current()?;
    // 先当表达式执行；报"顶层 return"语法错误时再包成异步函数重试。
    // 不能按是否含 "return" 判断，否则 location.href.includes('returnUrl') 这类表达式会丢失返回值。
    let mut outcome = eval_repl(browser, &session, script);
    // REPL 模式只允许和之前 eval 里的声明重名，和页面自己的全局 var/let/function 重名仍报"已声明"。
    // 这时把脚本包进代码块重试：块里的 const/let 只在块内有效，最后一个表达式照样是返回值，顶层 await 照样能用。
    // 只认执行前的声明检查报出的语法错误（check_exception 保留 "SyntaxError: " 前缀），这时第一次一条语句都没执行。
    // 不能在任意错误里找 "has already been declared"：脚本执行到一半时，被 reject 的 Promise、
    // 自定义 Error 也可能带这段文字，重试会让前面的点击、提交做两遍。
    // ponytail: 脚本调用的页面函数内部用 eval 在执行途中抛出同样的 SyntaxError 时认不出来，仍会重试；
    // 要排除得把 exceptionDetails 里有没有调用栈传出来，真遇到再做
    if outcome.as_ref().is_err_and(|error| {
        let message = error.to_string();
        message.starts_with("SyntaxError: Identifier '")
            && message.contains("has already been declared")
    }) {
        outcome = eval_repl(browser, &session, &format!("{{\n{script}\n}}"));
    }
    if outcome.as_ref().is_err_and(|error| {
        let message = error.to_string();
        message.contains("Illegal return") || message.contains("await is only valid")
    }) {
        let wrapped = format!("(async () => {{\n{script}\n}})()");
        outcome = eval_raw(browser, &session, &wrapped, true);
    }
    let mut output = match outcome {
        Ok(result) => {
            let value = result["result"]
                .get("value")
                .cloned()
                .unwrap_or(Value::Null);
            let serialized = serde_json::to_string(&value)?;
            let length = serialized.chars().count();
            if length > max {
                json!({
                    "ok": true,
                    "result": truncate(&serialized, max),
                    "truncated": true,
                    "length": length,
                    "hint": "结果超过 --max 被截成了字符串，不能当 JSON 解析：把 --max 加到 length 以上，或者先在页面里筛选、只返回需要的字段"
                })
            } else {
                json!({"ok": true, "result": value})
            }
        }
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    };
    // 很多拦截页是点筛选、翻页之后才跳出来的，这时 agent 接着调的是 eval，不经过 open。
    // 脚本报错时也带上：拦截页上读不到数据，报错的原因往往就是这个
    mark_blocked(browser, &session, &mut output);
    Ok(output)
}

pub fn screenshot(
    browser: &mut Browser,
    path: Option<&Path>,
    full: bool,
    annotate: bool,
) -> Result<Value> {
    let session = browser.attach_current()?;
    // 后台标签页不产生画面，直接截图会一直等不到结果，先切到前台
    browser
        .cdp
        .call("Page.bringToFront", json!({}), Some(&session))?;
    // 输出带上截图时的页面地址和标题，事后能核对这张图拍的是不是那个商品页、是不是拦截页
    let (url, title) = page_info(browser, &session)?;
    if annotate {
        eval_value(browser, &session, ANNOTATE_ADD)?;
    }
    // 路径以 .jpg/.jpeg 结尾时让 Chrome 直接出 JPEG：体积约为 PNG 的几分之一，适合当证据塞进报告
    let jpeg = path.is_some_and(|path| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg"))
    });
    let result = (|| {
        let mut params = if jpeg {
            json!({"format": "jpeg", "quality": 80})
        } else {
            json!({"format": "png"})
        };
        if full {
            let metrics = browser
                .cdp
                .call("Page.getLayoutMetrics", json!({}), Some(&session))?;
            // 旧版 Chrome 没有 cssContentSize，退回 contentSize，否则宽高是 null、截不出图
            let size = metrics
                .get("cssContentSize")
                .filter(|size| size.is_object())
                .unwrap_or(&metrics["contentSize"]);
            params["clip"] = json!({
                "x": 0,
                "y": 0,
                "width": size["width"],
                "height": size["height"],
                "scale": 1
            });
            params["captureBeyondViewport"] = json!(true);
        }
        browser
            .cdp
            .call("Page.captureScreenshot", params, Some(&session))
    })();
    if annotate {
        let _ = eval_value(browser, &session, ANNOTATE_REMOVE);
    }
    let result = result?;
    let taken_at = local_time_iso()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        result["data"]
            .as_str()
            .ok_or_else(|| anyhow!("Chrome 未返回截图数据"))?,
    )?;
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => browser.home.join("shots").join(format!(
            "{}.png",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        )),
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // 照旧覆盖，只在输出里说一声：同名文件被别的通道覆盖过，证据图就对不上了
    let overwrote = path.exists();
    fs::write(&path, &bytes)?;
    let (width, height) = if jpeg {
        jpeg_dimensions(&bytes)?
    } else {
        png_dimensions(&bytes)?
    };
    Ok(json!({
        "ok": true,
        "path": path,
        "width": width,
        "height": height,
        "url": url,
        "title": title,
        "taken_at": taken_at,
        "overwrote": overwrote
    }))
}

/// 本地时间，ISO 8601 格式带时区，如 2026-09-19T17:26:16+08:00。
/// 标准库不会取本地时区，借已经依赖的 SQLite 算，不为此加时间库
fn local_time_iso() -> Result<String> {
    Ok(rusqlite::Connection::open_in_memory()?.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime')
             || printf('%+03d:%02d', offset / 3600, abs(offset) / 60 % 60)
         FROM (SELECT strftime('%s', 'now', 'localtime') - strftime('%s', 'now') AS offset)",
        [],
        |row| row.get(0),
    )?)
}

pub struct WaitConditions<'a> {
    pub selector: Option<&'a str>,
    pub gone: Option<&'a str>,
    pub text: Option<&'a str>,
    pub url: Option<&'a str>,
    pub ms: Option<u64>,
    pub timeout_ms: u64,
}

pub fn wait(browser: &mut Browser, conditions: WaitConditions<'_>) -> Result<Value> {
    let session = browser.attach_current()?;
    if conditions.selector.is_none()
        && conditions.gone.is_none()
        && conditions.text.is_none()
        && conditions.url.is_none()
    {
        thread::sleep(Duration::from_millis(conditions.ms.unwrap_or(0)));
        return Ok(json!({"ok": true}));
    }
    let deadline = Instant::now() + Duration::from_millis(conditions.timeout_ms);
    let not_before = Instant::now() + Duration::from_millis(conditions.ms.unwrap_or(0));
    let expression = format!(
        // 可见判定用 click、fill 那一份（ELEMENT_HELPERS_JS，含 opacity > 0）：
        // 之前 wait 自己写了一份不看 opacity 的，元素还是 opacity:0 时 wait --selector 就放行，
        // 紧接着的 click 又报"不可见"。
        // script、style 这类元素从不渲染，按"存在"算：之前 wait --selector '#__NEXT_DATA__' 一律等满超时
        r#"(() => {{ {ELEMENT_HELPERS_JS}
        const inert = el => ['script', 'style', 'template', 'meta', 'link', 'title', 'noscript'].includes(el.localName);
        const shown = el => !!el && (inert(el) || visible(el));
        // 看所有命中里有没有可见的，不只看第一个：第一个是隐藏的模板时，--selector 永远等不到、--gone 会提前放行
        const any = s => [...document.querySelectorAll(s)].some(shown);
        const selector = {}, gone = {}, text = {}, url = {};
        return (!selector || any(selector)) && (!gone || !any(gone)) && (!text || (document.body?.innerText || '').includes(text)) && (!url || location.href.includes(url)); }})()"#,
        json!(conditions.selector),
        json!(conditions.gone),
        json!(conditions.text),
        json!(conditions.url)
    );
    while Instant::now() < deadline {
        if Instant::now() >= not_before
            && eval_value(browser, &session, &expression)?.as_bool() == Some(true)
        {
            return Ok(json!({"ok": true}));
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(json!({"ok": false, "error": "等待条件超时"}))
}

pub fn front(browser: &mut Browser) -> Result<Value> {
    // 先 attach：当前页为空时由它新建
    let session = browser.attach_current()?;
    let target = browser
        .state
        .current_target
        .clone()
        .ok_or_else(|| anyhow!("没有当前标签页"))?;
    browser
        .cdp
        .call("Target.activateTarget", json!({"targetId": target}), None)?;
    browser
        .cdp
        .call("Page.bringToFront", json!({}), Some(&session))?;
    Ok(json!({"ok": true}))
}

pub fn tabs(browser: &mut Browser) -> Result<Value> {
    let targets = browser.page_targets()?;
    let current = browser.state.current_target.as_deref();
    let tabs = targets
        .into_iter()
        .enumerate()
        .map(|(index, target)| {
            json!({
                "index": index,
                "targetId": target["targetId"],
                "url": target["url"],
                "title": target["title"],
                "current": target["targetId"].as_str() == current
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"ok": true, "tabs": tabs}))
}

pub fn tab(browser: &mut Browser, action: &str, value: Option<&str>) -> Result<Value> {
    if action == "new" {
        let url = normalize_url(value.unwrap_or("about:blank"))?;
        let result = browser
            .cdp
            .call("Target.createTarget", json!({"url": url}), None)?;
        let target = result["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("Chrome 未返回 targetId"))?
            .to_owned();
        browser.set_current(target.clone())?;
        browser
            .cdp
            .call("Target.activateTarget", json!({"targetId": target}), None)?;
        return Ok(json!({"ok": true, "targetId": target}));
    }
    let close = action == "close";
    let selector = if close { value } else { Some(action) };
    let target = if let Some(selector) = selector {
        resolve_tab(browser, selector)?
    } else {
        browser
            .state
            .current_target
            .clone()
            .ok_or_else(|| anyhow!("没有当前标签页"))?
    };
    if close {
        browser
            .cdp
            .call("Target.closeTarget", json!({"targetId": target}), None)?;
        // 关的是别的页时当前页不变，关的是当前页才重选。
        // 刚关的页会在 Target.getTargets 里短暂残留，重选时要排除它，否则又选回已关的页。
        // 剩下的页都是别的会话的当前页时不新建空白页，当前页留空（输出 current: null），
        // 等这个会话下一条命令连接时再选页或新建。多个会话收尾都只 tab close、之后不再使用时，
        // 新建的空白页记在它们留下的状态文件里，别的会话会跳过这些页，每轮都多出几个空白页。
        // 除了刚关的页一个标签页都不剩时照旧新建空白页，和之前一样，窗口不会因为没有标签页而关掉
        if browser.state.current_target.as_deref() == Some(target.as_str()) {
            let pages = browser.page_targets()?;
            browser.state.current_target = match browser.free_page(&pages, Some(&target)) {
                Some(free) => Some(free),
                None if pages.iter().any(|page| page["targetId"] != target.as_str()) => None,
                None => Some(browser.new_blank()?),
            };
            browser.save()?;
        }
        Ok(json!({"ok": true, "closed": target, "current": browser.state.current_target}))
    } else {
        browser.set_current(target.clone())?;
        browser
            .cdp
            .call("Target.activateTarget", json!({"targetId": target}), None)?;
        Ok(json!({"ok": true, "targetId": target}))
    }
}

pub fn back(browser: &mut Browser, timeout_ms: u64) -> Result<Value> {
    let session = browser.attach_current()?;
    drop_queued_load_events(browser, &session);
    eval_value(browser, &session, "history.back(); true")?;
    wait_navigated(browser, &session, timeout_ms)?;
    let loaded = wait_loaded(browser, &session, timeout_ms)?;
    let (url, title) = page_info(browser, &session)?;
    let mut output = json!({"ok": true, "url": url, "title": title, "loaded": loaded});
    mark_blocked(browser, &session, &mut output);
    Ok(output)
}

pub fn reload(browser: &mut Browser, timeout_ms: u64) -> Result<Value> {
    let session = browser.attach_current()?;
    drop_queued_load_events(browser, &session);
    browser.cdp.call("Page.reload", json!({}), Some(&session))?;
    wait_navigated(browser, &session, timeout_ms)?;
    let loaded = wait_loaded(browser, &session, timeout_ms)?;
    let (url, title) = page_info(browser, &session)?;
    let mut output = json!({"ok": true, "url": url, "title": title, "loaded": loaded});
    mark_blocked(browser, &session, &mut output);
    Ok(output)
}

pub fn status(browser: &mut Browser) -> Result<Value> {
    let pages = browser.page_targets()?;
    let current_url = pages.iter().find_map(|target| {
        (target["targetId"].as_str() == browser.state.current_target.as_deref())
            .then(|| target["url"].clone())
    });
    Ok(json!({
        "ok": true,
        "session": browser.session_name,
        "endpoint": browser.state.endpoint,
        "reachable": true,
        "launched": browser.state.launched,
        "tabs": pages.len(),
        "url": current_url
    }))
}

pub fn close(browser: &mut Browser) -> Result<Value> {
    if browser.state.launched {
        let _ = browser.cdp.call("Browser.close", json!({}), None);
    }
    browser.clear_state()?;
    Ok(json!({"ok": true, "browser_closed": browser.state.launched}))
}

/// by_value=true 时返回可序列化的值并等待 Promise；upload 需要元素本身（objectId），传 false。
fn eval_raw(
    browser: &mut Browser,
    session: &str,
    expression: &str,
    by_value: bool,
) -> Result<Value> {
    let result = browser.cdp.call(
        "Runtime.evaluate",
        eval_params(expression, by_value),
        Some(session),
    )?;
    check_exception(&result)?;
    Ok(result)
}

/// 按控制台（REPL）模式执行：同一页面上多次 eval 可以重复声明同名 const/let，顶层 await 直接拿到结果。
/// REPL 模式不会等表达式返回的 Promise（`fetch(u).then(r => r.json())` 会拿回空对象），
/// 所以结果是对象时再对它调一次函数，等 Promise 完成并按值取回。
fn eval_repl(browser: &mut Browser, session: &str, expression: &str) -> Result<Value> {
    let mut params = eval_params(expression, false);
    params["awaitPromise"] = json!(true);
    params["replMode"] = json!(true);
    let result = browser
        .cdp
        .call("Runtime.evaluate", params, Some(session))?;
    check_exception(&result)?;
    let Some(object_id) = result["result"]["objectId"].as_str() else {
        return Ok(result);
    };
    let result = browser.cdp.call(
        "Runtime.callFunctionOn",
        json!({
            "functionDeclaration": "function () { return this; }",
            "objectId": object_id,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true
        }),
        Some(session),
    )?;
    check_exception(&result)?;
    Ok(result)
}

fn eval_params(expression: &str, by_value: bool) -> Value {
    json!({
        "expression": expression,
        "returnByValue": by_value,
        "awaitPromise": by_value,
        "userGesture": true
    })
}

fn check_exception(result: &Value) -> Result<()> {
    let Some(details) = result.get("exceptionDetails") else {
        return Ok(());
    };
    // throw '文字' 抛的不是 Error 对象，没有 description，内容在 exception.value 里；
    // 只看 text 的话错误只剩一个 "Uncaught"
    let message = details["exception"]["description"]
        .as_str()
        .or_else(|| details["exception"]["value"].as_str())
        .or_else(|| details["text"].as_str())
        .unwrap_or("JavaScript 执行失败");
    // description 带调用栈，只保留第一行
    let first = message.lines().next().unwrap_or(message);
    bail!("{}", first.strip_prefix("Error: ").unwrap_or(first))
}

fn eval_value(browser: &mut Browser, session: &str, expression: &str) -> Result<Value> {
    let mut result = eval_raw(browser, session, expression, true)?;
    Ok(take_value(&mut result))
}

fn take_value(result: &mut Value) -> Value {
    result["result"]
        .get_mut("value")
        .map(Value::take)
        .unwrap_or(Value::Null)
}

fn value_string(value: Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("页面脚本未返回文字"))
}

/// `text=文字`：找整段文字（空白折叠后）恰好等于它的最内层可见元素。
/// 同一段文字出现在多处时，先排除被别的元素盖住的（弹窗后面的同名按钮、表格固定列下面那一份），
/// 再优先可点击的；还剩多个就报错列出来，不替 agent 猜，免得点错行。
const TEXT_TARGET_JS: &str = r#"((want, roots) => {
  const norm = s => (s || '').replace(/\s+/g, ' ').trim();
  want = norm(want);
  if (!want) throw new Error('text= 后面要写要找的文字，如 text=保存');
  const found = new Set();
  for (const root of roots) {
    const walker = (root.ownerDocument || root).createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let node; (node = walker.nextNode()); ) {
      const piece = norm(node.data);
      if (!piece || !want.includes(piece)) continue;
      // 文字可能拆在几个子元素里（图标 + 文字、分段高亮），往上找到整段文字刚好等于 want 的那一层
      for (let el = node.parentElement; el; el = el.parentElement) {
        const own = norm(el.innerText);
        if (own === want) { found.add(el); break; }
        if (own.length > want.length) break;
      }
    }
  }
  // visible、describe 定义在 target_expression 里
  const shown = [...found].filter(visible);
  if (!shown.length) throw new Error(`页面上没有文字恰好是"${want}"的可见元素（整段匹配，不做部分匹配）；先用 snapshot 或 text 看实际文字`);
  // 视口外的判断不了是否被盖住，保留
  const covered = el => {
    const r = el.getBoundingClientRect(), win = el.ownerDocument.defaultView;
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    if (x < 0 || y < 0 || x >= win.innerWidth || y >= win.innerHeight) return false;
    const hit = el.getRootNode().elementFromPoint(x, y);
    return !!hit && !el.contains(hit) && !hit.contains(el);
  };
  let pool = shown.filter(el => !covered(el));
  if (!pool.length) pool = shown;
  if (pool.length > 1) {
    const clickable = pool.filter(el => getComputedStyle(el).cursor === 'pointer' || el.closest('a[href],button,label,summary,select,[role=button],[role=tab],[role=link],[role=menuitem],[role=option],[role=checkbox],[role=radio],[role=switch],[role=treeitem]'));
    if (clickable.length) pool = clickable;
  }
  if (pool.length === 1) return pool[0];
  throw new Error(`文字"${want}"对应 ${pool.length} 个元素：${pool.slice(0, 5).map(describe).join('、')}，不确定是哪个；先 snapshot 用编号，或改用 CSS 选择器`);
})"#;

/// target_expression 里各段脚本共用。describe 不用 tagName.toLowerCase()：
/// 拦截页的脚本会改写 DOM 属性，实测有元素的 tagName 取到 undefined
const ELEMENT_HELPERS_JS: &str = r#"const visible = el => {
          const r = el.getBoundingClientRect(), s = getComputedStyle(el);
          return r.width > 0 && r.height > 0 && s.display !== 'none' && s.visibility !== 'hidden' && Number(s.opacity) > 0;
        };
        const describe = el => `<${el.localName || String(el.nodeName || '?')}${el.id ? '#' + el.id : ''}${el.classList?.length ? '.' + [...el.classList].slice(0, 3).join('.') : ''}>`;"#;

fn target_expression(target: &str, body: &str) -> String {
    let reference = target_ref(target);
    let text = target_text(target);
    format!(
        r#"(() => {{
        const target = {};
        const roots = [];
        const visit = root => {{ roots.push(root); for (const node of root.querySelectorAll('*')) {{ if (node.shadowRoot) visit(node.shadowRoot); if (node.tagName === 'IFRAME') try {{ if (node.contentDocument) visit(node.contentDocument); }} catch (_) {{}} }} }};
        visit(document);
        {ELEMENT_HELPERS_JS}
        let el = null;
        const ref = {};
        const text = {};
        if (ref) for (const root of roots) {{ el = root.querySelector(`[data-webctl-ref="${{ref}}"]`); if (el) break; }}
        else if (text !== null) el = {TEXT_TARGET_JS}(text, roots);
        else {{
          let all;
          try {{ all = [...document.querySelectorAll(target)]; }} catch (_) {{ throw new Error(`${{target}} 不是合法的 CSS 选择器；webctl 认 snapshot 编号 eN 和标准 CSS，按页面文字定位写 text=文字，不支持 :has-text() 这类 Playwright 写法`); }}
          // 同一个选择器常命中好几个（响应式页面桌面版、手机版各一份搜索框），先取第一个可见的；
          // 都不可见再退回第一个，隐藏的 input[type=file] 照样能 upload
          el = all.find(visible) || all[0];
        }}
        if (!el) throw new Error(ref ? `编号 ${{target}} 已失效，请重新 snapshot` : `找不到元素：${{target}}；先用 snapshot 拿编号 eN，别猜选择器；要按页面上的文字定位，写 text=文字`);
        {body}
        }})()"#,
        json!(target),
        json!(reference),
        json!(text)
    )
}

/// `text=保存`；Playwright 的 `text="保存"` 写法也认，去掉引号
fn target_text(target: &str) -> Option<&str> {
    let text = target.strip_prefix("text=")?.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return Some(inner);
        }
    }
    Some(text)
}

fn target_ref(target: &str) -> Option<&str> {
    let value = target.strip_prefix('@').unwrap_or(target);
    value
        .strip_prefix('e')
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        .then_some(value)
}

/// 挡住目标的往往是正在淡出的遮罩层或上一步悬停留下的提示框，等一下自己就没了。
/// 所以遮挡先重试一秒再报错；找不到元素之类的错误立刻返回，等也没用。
fn locate(browser: &mut Browser, session: &str, target: &str, force: bool) -> Result<(f64, f64)> {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    loop {
        let result = locate_once(browser, session, target, force);
        let retry = result
            .as_ref()
            .is_err_and(|error| format!("{error:#}").contains("遮挡"));
        if !retry || Instant::now() >= deadline {
            return result;
        }
        thread::sleep(Duration::from_millis(150));
    }
}

fn locate_once(
    browser: &mut Browser,
    session: &str,
    target: &str,
    force: bool,
) -> Result<(f64, f64)> {
    let body = format!(
        // behavior:'instant' 不能省：页面设了 scroll-behavior:smooth 时滚动会做动画，
        // 下一行量到的是动画中途的位置，点击会落到别处
        r#"el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}});
        const rect = el.getBoundingClientRect(), localX = rect.left + rect.width / 2, localY = rect.top + rect.height / 2;
        // 元素在开放 shadow root 里时，文档的 elementFromPoint 命中的是宿主元素，一律会误报遮挡；
        // 从元素自己的根（shadow root 或文档）取命中元素，和 TEXT_TARGET_JS 一致
        const root = el.getRootNode();
        const hit = (root.elementFromPoint ? root : el.ownerDocument).elementFromPoint(localX, localY);
        // 盖在目标上的如果是它的祖先，或和它同在一个 label 里（标题文字盖住 checkbox 是常见写法），
        // 点这个位置浏览器照样会作用到目标，不算遮挡
        const label = hit && hit.closest && hit.closest('label');
        const wraps = hit && (el.contains(hit) || hit.contains(el) || (label && (label.control === el || label.contains(el))));
        if (!{} && hit && hit !== el && !wraps) {{ const ref = hit.getAttribute('data-webctl-ref'); throw new Error(`被 ${{describe(hit)}}${{ref ? ' [' + ref + ']' : ''}} 遮挡；改点${{ref ? ' ' + ref : '遮挡它的元素'}}，或加 --force 强行点原位置`); }}
        let x = localX, y = localY, win = el.ownerDocument.defaultView;
        while (win && win !== top) {{ const frame = win.frameElement, r = frame.getBoundingClientRect(), s = getComputedStyle(frame); x += r.left + parseFloat(s.borderLeftWidth || 0) + parseFloat(s.paddingLeft || 0); y += r.top + parseFloat(s.borderTopWidth || 0) + parseFloat(s.paddingTop || 0); win = frame.ownerDocument.defaultView; }}
        return {{x, y}};"#,
        force
    );
    let result = eval_value(browser, session, &target_expression(target, &body))?;
    Ok((
        result["x"]
            .as_f64()
            .ok_or_else(|| anyhow!("无法定位元素横坐标"))?,
        result["y"]
            .as_f64()
            .ok_or_else(|| anyhow!("无法定位元素纵坐标"))?,
    ))
}

#[allow(clippy::too_many_arguments)]
fn mouse(
    browser: &mut Browser,
    session: &str,
    event_type: &str,
    x: f64,
    y: f64,
    button: &str,
    buttons: u8,
    click_count: u8,
) -> Result<()> {
    browser.cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": event_type, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": click_count}),
        Some(session),
    )?;
    Ok(())
}

/// 在 (x, y) 处按一次鼠标，和 `click` 用同一套真实鼠标事件序列。
/// `hold` 是按下到松开之间的毫秒数，不给就是普通点击的 30ms
fn click_buttons(
    browser: &mut Browser,
    session: &str,
    x: f64,
    y: f64,
    right: bool,
    double: bool,
    hold: Option<u64>,
) -> Result<()> {
    let button = if right { "right" } else { "left" };
    let buttons = if right { 2 } else { 1 };
    mouse(browser, session, "mousePressed", x, y, button, buttons, 1)?;
    thread::sleep(Duration::from_millis(hold.unwrap_or(30)));
    mouse(browser, session, "mouseReleased", x, y, button, 0, 1)?;
    if double {
        thread::sleep(Duration::from_millis(30));
        mouse(browser, session, "mousePressed", x, y, button, buttons, 2)?;
        thread::sleep(Duration::from_millis(30));
        mouse(browser, session, "mouseReleased", x, y, button, 0, 2)?;
    }
    Ok(())
}

/// 以拟人轨迹把鼠标移过去：二次贝塞尔曲线绕一点弯，速度先慢后快再慢，
/// 步间留几毫秒间隔。起点取本会话上一次移动停下的位置——webctl 每条命令
/// 都是新进程，鼠标位置存在 `<WEBCTL_HOME>/sessions/<会话>.mouse` 里；
/// 没有记录时从目标附近随机一点出发。
///
/// `pressed` 为 true 时移动事件带着按住的左键（button=left、buttons=1），给 drag 用。
/// `duration_ms` 给了就按这个总时长走完（每 16ms 左右一步），不给就每步 4–12ms。
fn human_move(
    browser: &mut Browser,
    session: &str,
    x: f64,
    y: f64,
    pressed: bool,
    duration_ms: Option<u64>,
) -> Result<()> {
    let (button, buttons) = if pressed { ("left", 1) } else { ("none", 0) };
    let mut rng = Rng::new();
    let from = read_mouse_pos(browser)
        .unwrap_or_else(|| (x + rng.range(-300.0, 300.0), y + rng.range(-200.0, 200.0)));
    let dx = x - from.0;
    let dy = y - from.1;
    let dist = dx.hypot(dy);
    if dist < 2.0 {
        mouse(browser, session, "mouseMoved", x, y, button, buttons, 0)?;
        write_mouse_pos(browser, x, y);
        return Ok(());
    }
    let mut steps = ((dist / 6.0) as usize).clamp(12, 48);
    if let Some(ms) = duration_ms {
        steps = steps.max((ms / 16) as usize);
    }
    // 控制点在中点垂线方向上随机偏一点，轨迹每次都略有不同
    let offset = rng.range(-0.25, 0.25) * dist;
    let cx = (from.0 + x) / 2.0 - dy / dist * offset;
    let cy = (from.1 + y) / 2.0 + dx / dist * offset;
    for i in 1..=steps {
        let t = ease_in_out(i as f64 / steps as f64);
        let u = 1.0 - t;
        let mut px = u * u * from.0 + 2.0 * u * t * cx + t * t * x;
        let mut py = u * u * from.1 + 2.0 * u * t * cy + t * t * y;
        if i != steps {
            // 途中加一点亚像素抖动，落点必须精确
            px += rng.range(-0.8, 0.8);
            py += rng.range(-0.8, 0.8);
        }
        mouse(browser, session, "mouseMoved", px, py, button, buttons, 0)?;
        let pause = match duration_ms {
            Some(ms) => ms as f64 / steps as f64 * rng.range(0.7, 1.3),
            None => rng.range(4.0, 12.0),
        };
        thread::sleep(Duration::from_millis(pause as u64));
    }
    write_mouse_pos(browser, x, y);
    Ok(())
}

fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn mouse_pos_path(browser: &Browser) -> PathBuf {
    browser
        .home
        .join("sessions")
        .join(format!("{}.mouse", browser.session_name))
}

fn read_mouse_pos(browser: &Browser) -> Option<(f64, f64)> {
    let text = fs::read_to_string(mouse_pos_path(browser)).ok()?;
    let (x, y) = text.trim().split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

/// 记位置失败不影响操作本身
fn write_mouse_pos(browser: &Browser, x: f64, y: f64) {
    let _ = fs::write(mouse_pos_path(browser), format!("{x},{y}"));
}

/// xorshift64* 伪随机数，种子取自时间和进程号，轨迹无需加密级随机
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Rng(nanos ^ (std::process::id() as u64) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// [lo, hi) 区间均匀分布
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        lo + (hi - lo) * unit
    }
}

fn begin_observe(browser: &mut Browser, session: &str) -> Result<HashSet<String>> {
    let targets = browser
        .page_targets()?
        .into_iter()
        .filter_map(|target| target["targetId"].as_str().map(str::to_owned))
        .collect();
    eval_value(
        browser,
        session,
        &format!("({OBSERVE_JS})({})", json!({"mode": "install"})),
    )?;
    Ok(targets)
}

fn finish_observe(
    browser: &mut Browser,
    session: &str,
    before_targets: HashSet<String>,
    settle_ms: u64,
    timeout_ms: u64,
) -> Result<Value> {
    thread::sleep(Duration::from_millis(300));
    // 把这段时间缓冲在连接里的事件（导航、加载、对话框）读出来
    browser
        .cdp
        .wait_event(|_| false, Duration::from_millis(10))?;

    let mut navigated_event = !browser
        .cdp
        .take_events(|event| is_main_frame_navigated(event, session))
        .is_empty();
    if !navigated_event {
        let deadline = Instant::now() + Duration::from_millis(settle_ms.max(300));
        let mut last: Option<Value> = None;
        let mut unchanged = Instant::now();
        while Instant::now() < deadline {
            // 页面跳转途中执行脚本会报错，当作"还在变化"继续等
            let probe = eval_value(browser, session, &observe_call("probe")).unwrap_or(Value::Null);
            if !browser
                .cdp
                .take_events(|event| is_main_frame_navigated(event, session))
                .is_empty()
            {
                navigated_event = true;
                break;
            }
            if last.as_ref() == Some(&probe) {
                if unchanged.elapsed() >= Duration::from_millis(300) {
                    break;
                }
            } else {
                last = Some(probe);
                unchanged = Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    if navigated_event {
        browser.cdp.wait_event(
            |event| event["sessionId"] == session && event["method"] == "Page.loadEventFired",
            Duration::from_millis(timeout_ms),
        )?;
    }
    // 跳转途中执行 collect 可能报错，按已跳转处理
    let observed = eval_value(browser, session, &observe_call("collect")).unwrap_or(Value::Null);
    let (url, title) = page_info(browser, session)?;
    // 页面换了新文档时，observe 的记录随旧文档一起丢失，collect 返回 null
    let navigated = navigated_event
        || observed.is_null()
        || observed["old_url"].as_str().is_some_and(|old| old != url);
    let text_changed = !navigated && observed["old_length"] != observed["length"];
    let added = if navigated {
        json!([])
    } else {
        observed.get("added").cloned().unwrap_or_else(|| json!([]))
    };
    let new_tabs = find_new_tabs(browser, &before_targets)?;
    let changed = navigated
        || text_changed
        || !new_tabs.is_empty()
        || added.as_array().is_some_and(|items| !items.is_empty());
    let mut output = json!({
        "ok": true,
        "changes": {
            "navigated": navigated,
            "url": url,
            "title": title,
            "new_tabs": new_tabs,
            "text_changed": text_changed,
            "added": added
        }
    });
    if changed {
        output["hint"] = json!("页面内容有变化，需要时重新 snapshot");
    } else {
        // 没检测到变化最容易被误读成"点击没生效"，而实际多半是表单提交、跳转比默认等待时间长
        output["hint"] = json!(
            "没检测到变化；慢的提交或跳转可能还没跑完，用 --settle 加长等待，或用 wait 等具体条件"
        );
    }
    Ok(output)
}

fn observe_call(mode: &str) -> String {
    format!("({OBSERVE_JS})({})", json!({"mode": mode}))
}

fn find_new_tabs(browser: &mut Browser, before: &HashSet<String>) -> Result<Vec<Value>> {
    Ok(browser
        .page_targets()?
        .iter()
        .filter(|target| {
            target["targetId"]
                .as_str()
                .is_some_and(|id| !before.contains(id))
        })
        .map(|target| json!({"targetId": target["targetId"], "url": target["url"]}))
        .collect())
}

fn is_main_frame_navigated(event: &Value, session: &str) -> bool {
    event["sessionId"] == session
        && event["method"] == "Page.frameNavigated"
        && event["params"]["frame"].get("parentId").is_none()
}

fn page_info(browser: &mut Browser, session: &str) -> Result<(String, String)> {
    let value = eval_value(
        browser,
        session,
        "({url: location.href, title: document.title})",
    )?;
    Ok((
        value["url"].as_str().unwrap_or_default().to_owned(),
        value["title"].as_str().unwrap_or_default().to_owned(),
    ))
}

/// 丢掉本会话队列里已有的 load 事件。webctl 接手前页面可能还在加载，attach、Page.enable 期间
/// 收到的是上一个文档的 load 事件；wait_loaded 先查队列，不丢就会立刻报 loaded:true。
/// 只能在发起导航之前丢：之后丢会把新文档真正的 load 事件一起丢掉
fn drop_queued_load_events(browser: &mut Browser, session: &str) {
    browser.cdp.take_events(|event| {
        event["sessionId"] == session && event["method"] == "Page.loadEventFired"
    });
}

/// DOM 就绪后最多再等多久 load 事件。广告多的站 DOM 3–7 秒就好了，load 要等图片、广告、第三方脚本，
/// 实测还要再晚 6–31 秒（2026-09-18，slickdeals、aliexpress、hlcwholesale 等）。
/// ponytail: 固定值；遇到 DOM 就绪后还要很久才渲染正文的站，调大它或让 agent 用 wait 等
const LOAD_GRACE: Duration = Duration::from_millis(3_000);

/// 等到 load 事件返回 true；DOM 就绪后超过 LOAD_GRACE 还没 load，或总时长到了 timeout，返回 false
fn wait_loaded(browser: &mut Browser, session: &str, timeout_ms: u64) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut grace_deadline: Option<Instant> = None;
    while Instant::now() < deadline && grace_deadline.is_none_or(|end| Instant::now() < end) {
        if browser
            .cdp
            .wait_event(
                |event| event["sessionId"] == session && event["method"] == "Page.loadEventFired",
                Duration::from_millis(50),
            )?
            .is_some()
        {
            return Ok(true);
        }
        // 跳转途中执行脚本会报"execution context destroyed"，当作还没就绪接着轮询，
        // 不能让这个临时错误把 open、back、reload 整条命令带失败
        match eval_value(browser, session, "document.readyState")
            .unwrap_or(Value::Null)
            .as_str()
        {
            Some("complete") => return Ok(true),
            Some("interactive") if grace_deadline.is_none() => {
                grace_deadline = Some(Instant::now() + LOAD_GRACE);
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(false)
}

/// history.back、reload 调用后旧文档的 readyState 仍是 complete，要先等主框架换成新文档
/// （单页应用的前进后退只触发 navigatedWithinDocument）。没有可后退的历史时最多等 5 秒。
fn wait_navigated(browser: &mut Browser, session: &str, timeout_ms: u64) -> Result<()> {
    browser.cdp.wait_event(
        |event| {
            event["sessionId"] == session
                && (event["method"] == "Page.navigatedWithinDocument"
                    || (event["method"] == "Page.frameNavigated"
                        && event["params"]["frame"].get("parentId").is_none()))
        },
        Duration::from_millis(timeout_ms.min(5_000)),
    )?;
    Ok(())
}

fn resolve_tab(browser: &mut Browser, selector: &str) -> Result<String> {
    let pages = browser.page_targets()?;
    if let Ok(index) = selector.parse::<usize>() {
        return pages
            .get(index)
            .and_then(|target| target["targetId"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("标签页序号不存在：{selector}"));
    }
    let matches = pages
        .iter()
        .filter_map(|target| target["targetId"].as_str())
        .filter(|id| id.starts_with(selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [id] => Ok((*id).to_owned()),
        [] => bail!("找不到 targetId 前缀：{selector}"),
        _ => bail!("targetId 前缀不唯一：{selector}"),
    }
}

#[derive(Debug, PartialEq)]
struct KeySpec {
    key: String,
    code: String,
    vk: u32,
    modifiers: u8,
    modifier_names: Vec<&'static str>,
    text: Option<String>,
}

fn parse_key(input: &str) -> Result<KeySpec> {
    let parts = input.split('+').collect::<Vec<_>>();
    let key_name = parts.last().copied().unwrap_or_default();
    let mut modifiers = 0;
    let mut modifier_names = Vec::new();
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        let (mask, name) = match modifier.to_ascii_lowercase().as_str() {
            "alt" | "option" => (1, "Alt"),
            "control" | "ctrl" => (2, "Control"),
            "meta" | "cmd" | "win" => (4, "Meta"),
            "shift" => (8, "Shift"),
            _ => bail!("不支持的修饰键：{modifier}；可用 Ctrl、Shift、Alt、Meta"),
        };
        modifiers |= mask;
        modifier_names.push(name);
    }
    let (key, code, vk, base_text) = match key_name.to_ascii_lowercase().as_str() {
        "enter" => ("Enter".into(), "Enter".into(), 13, Some("\r".into())),
        "tab" => ("Tab".into(), "Tab".into(), 9, None),
        "escape" => ("Escape".into(), "Escape".into(), 27, None),
        "backspace" => ("Backspace".into(), "Backspace".into(), 8, None),
        "delete" => ("Delete".into(), "Delete".into(), 46, None),
        "arrowup" => ("ArrowUp".into(), "ArrowUp".into(), 38, None),
        "arrowdown" => ("ArrowDown".into(), "ArrowDown".into(), 40, None),
        "arrowleft" => ("ArrowLeft".into(), "ArrowLeft".into(), 37, None),
        "arrowright" => ("ArrowRight".into(), "ArrowRight".into(), 39, None),
        "home" => ("Home".into(), "Home".into(), 36, None),
        "end" => ("End".into(), "End".into(), 35, None),
        "pageup" => ("PageUp".into(), "PageUp".into(), 33, None),
        "pagedown" => ("PageDown".into(), "PageDown".into(), 34, None),
        "space" => (" ".into(), "Space".into(), 32, Some(" ".into())),
        _ if key_name.len() == 1
            && key_name.is_ascii()
            && key_name.as_bytes()[0].is_ascii_alphanumeric() =>
        {
            let character = key_name.chars().next().unwrap();
            let upper = character.to_ascii_uppercase();
            let code = if character.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else {
                format!("Digit{character}")
            };
            (
                character.to_string(),
                code,
                upper as u32,
                Some(character.to_string()),
            )
        }
        _ => bail!("不支持的按键：{key_name}"),
    };
    let text = if modifiers & (1 | 2 | 4) == 0 {
        base_text
    } else {
        None
    };
    Ok(KeySpec {
        key,
        code,
        vk,
        modifiers,
        modifier_names,
        text,
    })
}

fn send_key(browser: &mut Browser, session: &str, spec: &KeySpec) -> Result<()> {
    let mut active = 0;
    for name in &spec.modifier_names {
        active |= modifier_mask(name);
        dispatch_key(
            browser,
            session,
            "rawKeyDown",
            name,
            &format!("{name}Left"),
            modifier_vk(name),
            active,
            None,
            None,
        )?;
    }
    let event_type = if spec.text.is_some() {
        "keyDown"
    } else {
        "rawKeyDown"
    };
    dispatch_key(
        browser,
        session,
        event_type,
        &spec.key,
        &spec.code,
        spec.vk,
        spec.modifiers,
        spec.text.as_deref(),
        None,
    )?;
    dispatch_key(
        browser,
        session,
        "keyUp",
        &spec.key,
        &spec.code,
        spec.vk,
        spec.modifiers,
        None,
        None,
    )?;
    for name in spec.modifier_names.iter().rev() {
        active &= !modifier_mask(name);
        dispatch_key(
            browser,
            session,
            "keyUp",
            name,
            &format!("{name}Left"),
            modifier_vk(name),
            active,
            None,
            None,
        )?;
    }
    Ok(())
}

fn select_all(browser: &mut Browser, session: &str) -> Result<()> {
    dispatch_key(
        browser,
        session,
        "rawKeyDown",
        "Control",
        "ControlLeft",
        17,
        2,
        None,
        None,
    )?;
    // commands: selectAll 让 Chrome 直接执行全选编辑命令，不依赖平台快捷键
    dispatch_key(
        browser,
        session,
        "keyDown",
        "a",
        "KeyA",
        65,
        2,
        None,
        Some(json!(["selectAll"])),
    )?;
    dispatch_key(browser, session, "keyUp", "a", "KeyA", 65, 2, None, None)?;
    dispatch_key(
        browser,
        session,
        "keyUp",
        "Control",
        "ControlLeft",
        17,
        0,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn dispatch_key(
    browser: &mut Browser,
    session: &str,
    event_type: &str,
    key: &str,
    code: &str,
    vk: u32,
    modifiers: u8,
    text: Option<&str>,
    commands: Option<Value>,
) -> Result<()> {
    let mut params = Map::new();
    params.insert("type".into(), json!(event_type));
    params.insert("key".into(), json!(key));
    params.insert("code".into(), json!(code));
    params.insert("windowsVirtualKeyCode".into(), json!(vk));
    params.insert("nativeVirtualKeyCode".into(), json!(vk));
    params.insert("modifiers".into(), json!(modifiers));
    if let Some(text) = text {
        params.insert("text".into(), json!(text));
    }
    if let Some(commands) = commands {
        params.insert("commands".into(), commands);
    }
    browser.cdp.call(
        "Input.dispatchKeyEvent",
        Value::Object(params),
        Some(session),
    )?;
    Ok(())
}

fn modifier_mask(name: &str) -> u8 {
    match name {
        "Alt" => 1,
        "Control" => 2,
        "Meta" => 4,
        "Shift" => 8,
        _ => 0,
    }
}

fn modifier_vk(name: &str) -> u32 {
    match name {
        "Alt" => 18,
        "Control" => 17,
        "Meta" => 91,
        "Shift" => 16,
        _ => 0,
    }
}

fn normalize_url(input: &str) -> Result<String> {
    let path = Path::new(input);
    if path.exists() {
        // 不用 canonicalize：Windows 上它返回 \\?\D:\... 形式，拼出的 file:// 地址无效
        let absolute = std::path::absolute(path)?;
        let mut value = absolute.to_string_lossy().replace('\\', "/");
        if !value.starts_with('/') {
            value.insert(0, '/');
        }
        return Ok(format!("file://{}", percent_encode_path(&value)));
    }
    if input.contains("://")
        || ["about:", "data:", "file:", "chrome:"]
            .iter()
            .any(|prefix| input.starts_with(prefix))
    {
        Ok(input.to_owned())
    } else if ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .any(|host| input.starts_with(host))
    {
        // 本机开发服务器一般没有 https
        Ok(format!("http://{input}"))
    } else {
        Ok(format!("https://{input}"))
    }
}

fn percent_encode_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b' '..=b'~' if !matches!(byte, b' ' | b'%' | b'#' | b'?') => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        text.to_owned()
    } else {
        format!(
            "{}\n… 已截断，总长度 {length}",
            text.chars().take(max).collect::<String>()
        )
    }
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        bail!("Chrome 返回的不是有效 PNG");
    }
    Ok((
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    ))
}

/// 从 JPEG 的 SOF 段读宽高；Chrome 出的是基线 JPEG，SOF0 就在前面几段里
fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 4 || bytes[..2] != [0xFF, 0xD8] {
        bail!("Chrome 返回的不是有效 JPEG");
    }
    let mut i = 2;
    while i + 9 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC3) {
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]);
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]);
            return Ok((width.into(), height.into()));
        }
        i += 2 + length;
    }
    bail!("JPEG 里找不到尺寸信息")
}

const ANNOTATE_ADD: &str = r#"(() => {
  document.getElementById('__webctl_overlay')?.remove();
  const overlay = document.createElement('div');
  overlay.id = '__webctl_overlay';
  overlay.style = 'position:absolute;left:0;top:0;z-index:2147483647;pointer-events:none';
  const roots = [];
  const visit = root => { roots.push(root); for (const el of root.querySelectorAll('*')) { if (el.shadowRoot) visit(el.shadowRoot); if (el.tagName === 'IFRAME') try { if (el.contentDocument) visit(el.contentDocument); } catch (_) {} } };
  visit(document);
  for (const root of roots) for (const el of root.querySelectorAll('[data-webctl-ref]')) {
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;
    let x = r.left, y = r.top, win = el.ownerDocument.defaultView;
    while (win && win !== top) { const frame = win.frameElement, fr = frame.getBoundingClientRect(), s = getComputedStyle(frame); x += fr.left + parseFloat(s.borderLeftWidth || 0) + parseFloat(s.paddingLeft || 0); y += fr.top + parseFloat(s.borderTopWidth || 0) + parseFloat(s.paddingTop || 0); win = frame.ownerDocument.defaultView; }
    x += scrollX; y += scrollY;
    const box = document.createElement('div');
    box.style = `position:absolute;left:${x}px;top:${y}px;width:${r.width}px;height:${r.height}px;border:2px solid #ff1744;box-sizing:border-box`;
    const label = document.createElement('span');
    label.textContent = el.getAttribute('data-webctl-ref');
    label.style = 'position:absolute;left:-2px;top:-18px;background:#ff1744;color:white;font:12px sans-serif;padding:2px';
    box.appendChild(label); overlay.appendChild(box);
  }
  document.documentElement.appendChild(overlay); return true;
})()"#;
const ANNOTATE_REMOVE: &str = "document.getElementById('__webctl_overlay')?.remove(); true";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jpeg_dimensions() {
        // SOI + APP0(长度 4) + SOF0：高 600、宽 800
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02,
            0x58, 0x03, 0x20, 0x03,
        ];
        assert_eq!(jpeg_dimensions(&bytes).unwrap(), (800, 600));
        assert!(jpeg_dimensions(b"\x89PNG").is_err());
    }

    #[test]
    fn reports_thrown_values() {
        // throw new Error(...)：内容在 exception.description 里，带调用栈，只取第一行
        let thrown_error = json!({"exceptionDetails": {
            "text": "Uncaught",
            "exception": {"description": "Error: 目标不可见\n    at <anonymous>:1:1"}
        }});
        assert_eq!(
            check_exception(&thrown_error).unwrap_err().to_string(),
            "目标不可见"
        );
        // throw '文字'：抛的不是 Error 对象，没有 description，内容在 exception.value 里
        let thrown_string = json!({"exceptionDetails": {
            "text": "Uncaught",
            "exception": {"type": "string", "value": "没有库存"}
        }});
        assert_eq!(
            check_exception(&thrown_string).unwrap_err().to_string(),
            "没有库存"
        );
        assert!(check_exception(&json!({"result": {"value": 1}})).is_ok());
    }

    #[test]
    fn parses_keys() {
        let control_a = parse_key("Control+A").unwrap();
        assert_eq!(control_a.modifiers, 2);
        assert_eq!(control_a.code, "KeyA");
        assert_eq!(control_a.text, None);
        let shift_tab = parse_key("Shift+Tab").unwrap();
        assert_eq!(shift_tab.modifiers, 8);
        assert_eq!(shift_tab.key, "Tab");
        let enter = parse_key("Enter").unwrap();
        assert_eq!(enter.text.as_deref(), Some("\r"));
    }

    #[test]
    fn identifies_targets() {
        assert_eq!(target_ref("e3"), Some("e3"));
        assert_eq!(target_ref("@e3"), Some("e3"));
        assert_eq!(target_ref("#save"), None);
        assert_eq!(target_text("text=保存"), Some("保存"));
        assert_eq!(target_text(r#"text="新增物流渠道""#), Some("新增物流渠道"));
        assert_eq!(target_text("text='NUS-US'"), Some("NUS-US"));
        assert_eq!(target_text("#save"), None);
    }

    #[test]
    fn completes_urls() {
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(
            normalize_url("https://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_url("localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
        // 测试在包根目录运行，Cargo.toml 一定存在；Windows 上不能带 \\?\ 前缀
        let local = normalize_url("Cargo.toml").unwrap();
        assert!(
            local.starts_with("file:///")
                && local.ends_with("/Cargo.toml")
                && !local.contains("%3F"),
            "{local}"
        );
    }
}
