mod browser;
mod cdp;
mod log;
mod page;
mod vision;

use anyhow::{Context, Result, bail};
use browser::Browser;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Instant};

#[derive(Parser)]
#[command(
    name = "webctl",
    version,
    about = "供 agent 使用的 Chrome 命令行控制工具"
)]
struct Cli {
    #[arg(long, global = true)]
    session: Option<String>,
    #[arg(long, global = true)]
    cdp: Option<String>,
    #[arg(long, global = true)]
    headless: bool,
    /// 页面弹出 alert/confirm/prompt/beforeunload 对话框时点确定（accept）还是取消（dismiss）
    #[arg(long, global = true, value_parser = ["accept", "dismiss"], default_value = "dismiss")]
    on_dialog: String,
    /// prompt 对话框的输入内容，配合 --on-dialog accept
    #[arg(long, global = true)]
    prompt_text: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 打开网址（本地文件路径也可以），返回 url、title；认出拦截页时带 blocked
    ///
    /// DOM 就绪后最多再等 3 秒 load 事件，没等到就先返回 "loaded": false，正文一般已经在了。要等某段内容出来，接着用 `wait --text 文字` 或 `wait --selector CSS`，不要固定 sleep。不带 URL 时只确保浏览器已启动。
    Open {
        /// 网址或本地文件路径；没写协议时补 https://
        url: Option<String>,
        /// 新开一个标签页打开并设为当前页（会切到前台）；看大图这类临时页面用完 `tab close`
        #[arg(long)]
        new_tab: bool,
        /// 等加载的最长毫秒数，超时不算失败，返回 "loaded": false
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Snapshot {
        #[arg(long = "in")]
        within: Option<String>,
        #[arg(long, default_value_t = 300)]
        max: usize,
    },
    /// 读整页或某个元素的文字（纯文本输出）
    Text {
        /// 只读这个元素里的文字：snapshot 编号、CSS 选择器或 text=文字，如 `text main`、`text "#price"`；不写读整页
        target: Option<String>,
        /// 最多输出多少字，超出截断并注明总长度
        #[arg(long, default_value_t = 15_000)]
        max: usize,
    },
    Click {
        target: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        right: bool,
        #[arg(long = "double")]
        double_click: bool,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Fill {
        target: String,
        text: String,
        #[arg(long)]
        append: bool,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Type {
        text: String,
        #[arg(long, default_value_t = 0)]
        delay: u64,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Press {
        key: String,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Select {
        target: String,
        value: String,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Upload {
        target: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    Hover {
        target: String,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Scroll {
        direction: String,
        pixels: Option<i64>,
        #[arg(long = "in")]
        within: Option<String>,
    },
    /// 在当前页面执行 JavaScript，返回结果；认出拦截页时带 blocked
    ///
    /// 可以写 return、顶层 await；同一页面上多次 eval 可以重复声明同名 const。多行脚本、含 $ 或 \ 的脚本先写进文件再用 --file，不要用 "$(cat 文件)" 传：shell 会改掉里面的字符。
    Eval {
        /// 要执行的 JS（和 --file 二选一）
        js: Option<String>,
        /// 从文件读取要执行的 JS
        #[arg(long)]
        file: Option<PathBuf>,
        /// 结果序列化成 JSON 后最多多少字，超出截成字符串并标 "truncated": true 和完整长度 "length"
        #[arg(long, default_value_t = 20_000)]
        max: usize,
    },
    /// 截图（先把当前标签页切到前台），返回 path、宽高、url、title、taken_at、overwrote
    ///
    /// 例：`webctl screenshot shots/item-173015.jpg`。文件已存在时照样覆盖，返回 "overwrote": true。
    Screenshot {
        /// 保存路径，直接写在命令后面（位置参数，没有 --path、-o）；.jpg/.jpeg 结尾存 JPEG，否则 PNG；不写存到数据目录的 shots/ 下
        path: Option<PathBuf>,
        /// 截整个页面，不只是当前视口
        #[arg(long)]
        full: bool,
        /// 在图上框出 snapshot 编号
        #[arg(long)]
        annotate: bool,
    },
    /// 等条件全部满足（每 250ms 查一次）；open、click 之后等内容出来用它，不要固定 sleep
    Wait {
        /// 等这个 CSS 选择器有可见的命中
        #[arg(long)]
        selector: Option<String>,
        /// 等这个 CSS 选择器没有可见的命中
        #[arg(long)]
        gone: Option<String>,
        /// 等页面文字里出现这段文字
        #[arg(long)]
        text: Option<String>,
        /// 等地址里出现这个片段
        #[arg(long)]
        url: Option<String>,
        /// 至少等这么多毫秒再检查条件；不带其他条件时就是固定等待
        #[arg(long)]
        ms: Option<u64>,
        /// 最长等多少毫秒，超时返回 ok:false
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    /// 点一下交互式人机验证的勾选框（和真人点同一个动作）；非交互式质询不动，如实报出
    Turnstile {
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    /// 视觉识别：在当前页面截图里找模板图片，返回匹配中心的视口坐标
    Find {
        image: PathBuf,
        #[arg(long, default_value_t = 0.8)]
        threshold: f64,
        #[arg(long, default_value_t = 1)]
        max: usize,
    },
    /// 以拟人轨迹把鼠标移到视口坐标 (X, Y)，只移动不点击
    Move {
        x: f64,
        y: f64,
    },
    /// 移动并点击视口坐标 (X, Y)，不经过 DOM 定位，可点 canvas 等 JS 够不着的目标
    ///
    /// 加 `--hold 毫秒` 是长按：拟人轨迹移过去，按下，保持这么久，再松开。
    Clickat {
        x: f64,
        y: f64,
        #[arg(long)]
        right: bool,
        #[arg(long = "double")]
        double_click: bool,
        /// 按下后保持多少毫秒再松开（长按）；不写就是普通点击
        #[arg(long, conflicts_with = "double_click")]
        hold: Option<u64>,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    /// 按住左键从视口坐标 (X1, Y1) 拖到 (X2, Y2)，用于滑块、拖拽排序等
    ///
    /// 拟人轨迹移到起点，按下左键，按住左键沿拟人轨迹移到终点（移动事件带 buttons=1），松开。只按给定坐标操作，不识别页面内容。
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        /// 从起点拖到终点用多少毫秒
        #[arg(long, default_value_t = 800)]
        duration: u64,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    /// 视觉识别 + 点击：找模板图片在页面上的位置，以拟人轨迹移过去点击
    Vclick {
        image: PathBuf,
        #[arg(long, default_value_t = 0.8)]
        threshold: f64,
        #[arg(long)]
        right: bool,
        #[arg(long = "double")]
        double_click: bool,
        #[arg(long, default_value_t = 3_000)]
        settle: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Front,
    /// 列出标签页（序号从 0 开始）
    Tabs,
    /// 标签页操作：`tab 序号` 切换、`tab new [URL]` 新建、`tab close [序号]` 关闭（缺省关当前页）
    Tab {
        /// 序号、targetId 前缀，或 new / close
        action: String,
        /// new 的 URL，或 close 的序号
        value: Option<String>,
    },
    Back {
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Reload {
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Status,
    /// 关闭整个浏览器（本会话所有标签页）；只关一个标签页用 `tab close [序号]`
    Close,
}

enum Output {
    Json(Value),
    Text(String),
}

fn main() {
    // 先取 matches 是为了拿到子命令名写进日志，再照常解析成 Cli
    let matches = Cli::command().get_matches();
    let command_name = matches.subcommand_name().unwrap_or("?").to_owned();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let session = session_name(cli.session.clone());
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let started = Instant::now();

    let result = run(cli, &session);
    let (ok, error) = match &result {
        // 只有显式的 ok:false 算失败，和下面的退出码保持一致
        Ok(Output::Json(value)) => (
            value["ok"] != false,
            value["error"].as_str().map(str::to_owned),
        ),
        Ok(Output::Text(_)) => (true, None),
        Err(error) => (false, Some(format!("{error:#}"))),
    };
    log::record(log::Record {
        session: &session,
        command: &command_name,
        argv: &argv,
        ok,
        error: error.as_deref(),
        elapsed: started.elapsed(),
        output: match &result {
            Ok(Output::Json(value)) => Some(value),
            _ => None,
        },
    });

    // 失败时 stdout 照旧输出 JSON，另往 stderr 写一行：
    // snapshot、text 的输出常接 `| grep`，只写 stdout 的话错误行会被过滤掉，看起来像页面上没有
    if !ok {
        eprintln!("webctl: {}", error.as_deref().unwrap_or("命令失败"));
    }
    match result {
        Ok(Output::Json(value)) if value["ok"] == false => {
            println!("{value}");
            std::process::exit(1);
        }
        Ok(Output::Json(value)) => println!("{value}"),
        Ok(Output::Text(text)) => println!("{text}"),
        Err(error) => {
            println!("{}", json!({"ok": false, "error": format!("{error:#}")}));
            std::process::exit(1);
        }
    }
}

fn session_name(flag: Option<String>) -> String {
    flag.or_else(|| std::env::var("WEBCTL_SESSION").ok())
        .unwrap_or_else(|| "default".to_owned())
}

fn run(cli: Cli, session: &str) -> Result<Output> {
    // status、close 只查询已有会话，浏览器没在运行时不能为此去启动一个
    if cli.cdp.is_none()
        && matches!(cli.command, Command::Status | Command::Close)
        && !browser::session_reachable(session)?
    {
        return Ok(Output::Json(
            json!({"ok": true, "session": session, "reachable": false}),
        ));
    }
    let mut browser = Browser::connect(session, cli.cdp.as_deref(), cli.headless)?;
    browser.cdp.accept_dialogs = cli.on_dialog == "accept";
    browser.cdp.prompt_text = cli.prompt_text;
    let output = dispatch(&mut browser, cli.command);
    with_dialogs(output, std::mem::take(&mut browser.cdp.dialogs))
}

fn dispatch(browser: &mut Browser, command: Command) -> Result<Output> {
    let output = match command {
        Command::Open {
            url,
            new_tab,
            timeout,
        } => page::open(browser, url.as_deref(), new_tab, timeout)?,
        Command::Snapshot { within, max } => {
            return Ok(Output::Text(page::snapshot(
                browser,
                within.as_deref(),
                max,
            )?));
        }
        Command::Text { target, max } => {
            return Ok(Output::Text(page::text(browser, target.as_deref(), max)?));
        }
        Command::Click {
            target,
            force,
            right,
            double_click,
            settle,
            timeout,
        } => page::click(
            browser,
            &target,
            force,
            right,
            double_click,
            settle,
            timeout,
        )?,
        Command::Fill {
            target,
            text,
            append,
            settle,
            timeout,
        } => page::fill(browser, &target, &text, append, settle, timeout)?,
        Command::Type {
            text,
            delay,
            settle,
            timeout,
        } => page::type_text(browser, &text, delay, settle, timeout)?,
        Command::Press {
            key,
            settle,
            timeout,
        } => page::press(browser, &key, settle, timeout)?,
        Command::Select {
            target,
            value,
            settle,
            timeout,
        } => page::select(browser, &target, &value, settle, timeout)?,
        Command::Upload { target, files } => page::upload(browser, &target, &files)?,
        Command::Hover {
            target,
            settle,
            timeout,
        } => page::hover(browser, &target, settle, timeout)?,
        Command::Scroll {
            direction,
            pixels,
            within,
        } => page::scroll(browser, &direction, pixels, within.as_deref())?,
        Command::Eval { js, file, max } => {
            if js.is_some() == file.is_some() {
                bail!("eval 必须且只能提供 JS 参数或 --file");
            }
            let script = match file {
                Some(path) => fs::read_to_string(&path)
                    .with_context(|| format!("无法读取 {}", path.display()))?,
                None => js.unwrap(),
            };
            page::evaluate(browser, &script, max)?
        }
        Command::Screenshot {
            path,
            full,
            annotate,
        } => page::screenshot(browser, path.as_deref(), full, annotate)?,
        Command::Wait {
            selector,
            gone,
            text,
            url,
            ms,
            timeout,
        } => page::wait(
            browser,
            page::WaitConditions {
                selector: selector.as_deref(),
                gone: gone.as_deref(),
                text: text.as_deref(),
                url: url.as_deref(),
                ms,
                timeout_ms: timeout,
            },
        )?,
        Command::Find {
            image,
            threshold,
            max,
        } => page::find_image(browser, &image, threshold, max)?,
        Command::Move { x, y } => page::move_to(browser, x, y)?,
        Command::Clickat {
            x,
            y,
            right,
            double_click,
            hold,
            settle,
            timeout,
        } => page::click_at(browser, x, y, right, double_click, hold, settle, timeout)?,
        Command::Drag {
            x1,
            y1,
            x2,
            y2,
            duration,
            settle,
            timeout,
        } => page::drag(browser, (x1, y1), (x2, y2), duration, settle, timeout)?,
        Command::Vclick {
            image,
            threshold,
            right,
            double_click,
            settle,
            timeout,
        } => page::vclick(
            browser,
            &image,
            threshold,
            right,
            double_click,
            settle,
            timeout,
        )?,
        Command::Turnstile { timeout } => page::turnstile(browser, timeout)?,
        Command::Front => page::front(browser)?,
        Command::Tabs => page::tabs(browser)?,
        Command::Tab { action, value } => page::tab(browser, &action, value.as_deref())?,
        Command::Back { timeout } => page::back(browser, timeout)?,
        Command::Reload { timeout } => page::reload(browser, timeout)?,
        Command::Status => page::status(browser)?,
        Command::Close => page::close(browser)?,
    };
    Ok(Output::Json(output))
}

/// 把本条命令里处理过的对话框附在输出里，让 agent 知道发生过什么、需要时换 --on-dialog 重做
fn with_dialogs(output: Result<Output>, dialogs: Vec<Value>) -> Result<Output> {
    if dialogs.is_empty() {
        return output;
    }
    let dismissed = dialogs
        .iter()
        .any(|dialog| dialog["handled"] == "dismiss" && dialog["type"] != "alert");
    let hint = "页面弹出的对话框已按取消处理；确认要执行时加 --on-dialog accept 重新操作";
    Ok(match output {
        Ok(Output::Json(mut value)) => {
            value["dialogs"] = json!(dialogs);
            if dismissed {
                value["hint"] = json!(match value["hint"].as_str() {
                    Some(existing) => format!("{existing}；{hint}"),
                    None => hint.to_owned(),
                });
            }
            Output::Json(value)
        }
        Ok(Output::Text(mut text)) => {
            for dialog in &dialogs {
                text.push_str(&format!(
                    "\n[对话框] {} {} 已按 {} 处理",
                    dialog["type"].as_str().unwrap_or("?"),
                    dialog["message"],
                    dialog["handled"].as_str().unwrap_or("?")
                ));
            }
            Output::Text(text)
        }
        Err(error) => {
            Output::Json(json!({"ok": false, "error": format!("{error:#}"), "dialogs": dialogs}))
        }
    })
}
