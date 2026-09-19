use crate::cdp::Cdp;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub endpoint: String,
    pub launched: bool,
    pub pid: Option<u32>,
    pub current_target: Option<String>,
    /// 标签页首次出现的先后顺序，`tabs` 的序号按它排
    #[serde(default)]
    pub tab_order: Vec<String>,
}

pub struct Browser {
    pub cdp: Cdp,
    pub state: SessionState,
    pub session_name: String,
    pub home: PathBuf,
    state_path: PathBuf,
}

impl Browser {
    pub fn connect(session_name: &str, supplied_cdp: Option<&str>, headless: bool) -> Result<Self> {
        validate_session_name(session_name)?;
        let home = data_home()?;
        let state_path = home.join("sessions").join(format!("{session_name}.json"));
        let mut state = if let Some(cdp) = supplied_cdp {
            SessionState {
                endpoint: normalize_endpoint(cdp)?,
                launched: false,
                pid: None,
                current_target: None,
                tab_order: Vec::new(),
            }
        } else if let Ok(text) = fs::read_to_string(&state_path) {
            serde_json::from_str(&text).context("会话状态文件格式错误")?
        } else {
            launch_chrome(&home, session_name, headless)?
        };

        let cdp = match endpoint_ws(&state.endpoint).and_then(|url| Cdp::connect(&url)) {
            Ok(cdp) => cdp,
            Err(_) if supplied_cdp.is_none() => {
                state = launch_chrome(&home, session_name, headless)?;
                Cdp::connect(&endpoint_ws(&state.endpoint)?)?
            }
            Err(error) => return Err(error),
        };
        let mut browser = Self {
            cdp,
            state,
            session_name: session_name.to_owned(),
            home,
            state_path,
        };
        browser.choose_current_target()?;
        browser.save()?;
        Ok(browser)
    }

    pub fn targets(&mut self) -> Result<Vec<Value>> {
        Ok(
            self.cdp.call("Target.getTargets", json!({}), None)?["targetInfos"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        )
    }

    pub fn page_targets(&mut self) -> Result<Vec<Value>> {
        let mut pages: Vec<Value> = self
            .targets()?
            .into_iter()
            .filter(|target| {
                target["type"] == "page"
                    && !target["url"].as_str().is_some_and(|url| {
                        url.starts_with("devtools://") || url.starts_with("chrome-extension://")
                    })
            })
            .collect();
        order_pages(&mut self.state.tab_order, &mut pages);
        Ok(pages)
    }

    pub fn attach_current(&mut self) -> Result<String> {
        if self.state.current_target.is_none() {
            let blank = self.new_blank()?;
            self.set_current(blank)?;
        }
        let target = self
            .state
            .current_target
            .as_deref()
            .ok_or_else(|| anyhow!("没有当前标签页"))?;
        let result = self.cdp.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )?;
        let session = result["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("Chrome 未返回 sessionId"))?
            .to_owned();
        // 页面在 webctl 连上之前就弹出了对话框时，页面脚本停住，Page.enable 不会返回；
        // 这种对话框也无法通过 CDP 关闭，只能由人在浏览器里处理
        self.cdp
            .call_timeout(
                "Page.enable",
                json!({}),
                Some(&session),
                Duration::from_secs(5),
            )
            .context(
                "页面没有响应，可能开着一个 webctl 接手前弹出的对话框；有界面时用 `webctl front` 切到前台手动关闭",
            )?;
        // 标签页退到后台（页面自己开了新标签、或用户切到别的程序）时，Chrome 会把鼠标事件压住约 5 秒
        // 再放行。让渲染进程始终认为自己有焦点，就不必真去抢窗口前台——抢也抢不到，Windows 不让后台
        // 程序改前台窗口。旧版 Chrome 没有这个命令，失败了忽略。
        let _ = self.cdp.call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
            Some(&session),
        );
        Ok(session)
    }

    pub fn set_current(&mut self, target: String) -> Result<()> {
        self.state.current_target = Some(target);
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.state_path, serde_json::to_vec(&self.state)?)?;
        Ok(())
    }

    pub fn clear_state(&self) -> Result<()> {
        match fs::remove_file(&self.state_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn choose_current_target(&mut self) -> Result<()> {
        let pages = self.page_targets()?;
        let exists = self.state.current_target.as_ref().is_some_and(|current| {
            pages
                .iter()
                .any(|target| target["targetId"].as_str() == Some(current))
        });
        // 没有可选的页时当前页留空，等 attach_current 真要用页面时再新建：
        // 多个通道按 concurrency.md 用 `--cdp … tab new` 初始化时，连接时先建的空白页马上被 tab new 替下，
        // 没人再用，每轮都剩下几个（2026-09-19 七个通道收尾后剩 2 个）
        if !exists {
            self.state.current_target = self.free_page(None)?;
        }
        Ok(())
    }

    /// 找一个能当当前页的标签页：跳过 `exclude`（刚关掉、还在列表里残留的页）和同一个 Chrome 上
    /// 其他会话的当前页，找不到返回 None，不新建。
    /// 多个会话共用一个 Chrome 时，直接取第一个标签页会选中别的会话正在用的页，之后的 open 会把它导航走。
    /// 只有一个会话时没有要跳过的页，第一次连上照旧接管用户已经打开的标签页。
    pub fn free_page(&mut self, exclude: Option<&str>) -> Result<Option<String>> {
        let taken = self.other_sessions_targets();
        Ok(self.page_targets()?.into_iter().find_map(|page| {
            page["targetId"]
                .as_str()
                .filter(|id| Some(*id) != exclude && !taken.iter().any(|other| other == id))
                .map(str::to_owned)
        }))
    }

    pub fn new_blank(&mut self) -> Result<String> {
        let result = self
            .cdp
            .call("Target.createTarget", json!({"url": "about:blank"}), None)?;
        result["targetId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Chrome 未返回 targetId"))
    }

    /// `<数据目录>/sessions/` 下其他会话状态文件里、endpoint 和本会话相同的 current_target。
    /// ponytail: 按状态文件判断，会话不用了但文件还在时它的页也会被跳过，下一条要用页面的命令会多开一个 about:blank；
    /// 同一个 Chrome 用不同写法的地址连（localhost 和 127.0.0.1）时认不出是同一个
    fn other_sessions_targets(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.home.join("sessions")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                *path != self.state_path && path.extension().is_some_and(|ext| ext == "json")
            })
            .filter_map(|path| fs::read_to_string(path).ok())
            .filter_map(|text| serde_json::from_str::<SessionState>(&text).ok())
            .filter(|state| state.endpoint == self.state.endpoint)
            .filter_map(|state| state.current_target)
            .collect()
    }
}

/// Target.getTargets 按最近激活排序，切一次标签页序号就变；改按首次见到的先后排，新标签页排在最后
fn order_pages(order: &mut Vec<String>, pages: &mut [Value]) {
    let id = |page: &Value| page["targetId"].as_str().unwrap_or_default().to_owned();
    order.retain(|known| pages.iter().any(|page| id(page) == *known));
    for page in pages.iter() {
        if !order.contains(&id(page)) {
            order.push(id(page));
        }
    }
    pages.sort_by_key(|page| order.iter().position(|known| *known == id(page)));
}

pub fn data_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("WEBCTL_HOME") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = env::var_os("HOME").map(|path| PathBuf::from(path).join(".webctl"));
    #[cfg(windows)]
    return base
        .map(|path| path.join("webctl"))
        .ok_or_else(|| anyhow!("未设置 LOCALAPPDATA 或 WEBCTL_HOME"));
    #[cfg(not(windows))]
    base.ok_or_else(|| anyhow!("未设置 HOME 或 WEBCTL_HOME"))
}

fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        bail!("会话名只能包含字母、数字、-、_ 和 .");
    }
    Ok(())
}

fn normalize_endpoint(value: &str) -> Result<String> {
    if value.starts_with("ws://") || value.starts_with("http://") {
        Ok(value.trim_end_matches('/').to_owned())
    } else if value.chars().all(|character| character.is_ascii_digit()) {
        Ok(format!("http://127.0.0.1:{value}"))
    } else {
        bail!("--cdp 必须是端口、http:// 或 ws:// 地址")
    }
}

fn endpoint_ws(endpoint: &str) -> Result<String> {
    if endpoint.starts_with("ws://") {
        return Ok(endpoint.to_owned());
    }
    let version = http_json(endpoint, "/json/version", Duration::from_secs(1))?;
    version["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("/json/version 没有 webSocketDebuggerUrl"))
}

fn http_json(endpoint: &str, path: &str, timeout: Duration) -> Result<Value> {
    let authority = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("只支持本机 http:// CDP 地址"))?
        .split('/')
        .next()
        .unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("CDP 地址缺少端口"))?;
    let address = (host, port.parse::<u16>()?)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("无法解析 CDP 地址"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // Chrome 调试端口不接受 HTTP/1.0，收到后直接断开连接，必须用 HTTP/1.1
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    let mut buffer = [0; 4096];
    let (separator, content_length) = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            bail!("Chrome 提前关闭了 HTTP 响应");
        }
        response.extend_from_slice(&buffer[..read]);
        if let Some(separator) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&response[..separator]);
            let content_length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                })
                .map(|(_, value)| value.trim().parse::<usize>())
                .transpose()?
                .ok_or_else(|| anyhow!("Chrome HTTP 响应缺少 Content-Length"))?;
            if response.len() >= separator + 4 + content_length {
                break (separator, content_length);
            }
        }
    };
    let header = String::from_utf8_lossy(&response[..separator]);
    if !header
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        bail!(
            "Chrome 调试端口返回非 200 响应：{}",
            header.lines().next().unwrap_or("")
        );
    }
    Ok(serde_json::from_slice(
        &response[separator + 4..separator + 4 + content_length],
    )?)
}

fn launch_chrome(home: &Path, session_name: &str, headless: bool) -> Result<SessionState> {
    let executable = find_chrome().ok_or_else(|| {
        anyhow!("找不到 Chrome 或 Edge；请设置环境变量 WEBCTL_CHROME 指向浏览器可执行文件")
    })?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let profile = home.join("profiles").join(session_name);
    fs::create_dir_all(&profile)?;
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        "--hide-crash-restore-bubble".to_owned(),
    ];
    if headless {
        args.push("--headless=new".to_owned());
    }
    let pid = spawn_browser(&executable, &args)?;
    let endpoint = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if endpoint_ws(&endpoint).is_ok() {
            return Ok(SessionState {
                endpoint,
                launched: true,
                pid: Some(pid),
                current_target: None,
                tab_order: Vec::new(),
            });
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "Chrome 启动后 15 秒内未开放调试端口；若该配置目录已有 Chrome 在运行，请先关闭后重试，或用 --cdp 连接"
    )
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WEBCTL_CHROME").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env::var_os(variable).map(PathBuf::from) {
                candidates.push(base.join("Google/Chrome/Application/chrome.exe"));
                candidates.push(base.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ]);
    #[cfg(all(unix, not(target_os = "macos")))]
    candidates.extend(
        [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/microsoft-edge",
        ]
        .map(PathBuf::from),
    );
    candidates.into_iter().find(|path| path.is_file())
}

/// status、close 只查询已有会话，不应为此启动浏览器。
pub fn session_reachable(session_name: &str) -> Result<bool> {
    validate_session_name(session_name)?;
    let path = data_home()?
        .join("sessions")
        .join(format!("{session_name}.json"));
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let state: SessionState = serde_json::from_str(&text).context("会话状态文件格式错误")?;
    Ok(endpoint_ws(&state.endpoint)
        .and_then(|url| Cdp::connect(&url))
        .is_ok())
}

/// 启动浏览器进程，不让它继承 webctl 的任何句柄。
///
/// std::process::Command 在 Windows 上会让子进程继承本进程所有可继承的句柄，其中包括调用方
/// （agent、测试程序）传给 webctl 的输出管道，以及 webctl 从更上层进程继承来的管道。
/// Chrome 长期运行并一直持有这些管道，调用方就读不到输出结束，命令看起来一直不返回。
/// 只清除 webctl 自己标准句柄的继承标志不够，所以直接调 CreateProcessW 并关闭句柄继承。
#[cfg(windows)]
fn spawn_browser(executable: &Path, args: &[String]) -> Result<u32> {
    use std::{
        ffi::c_void,
        ptr::{null, null_mut},
    };

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_size: u16,
        reserved2: *mut u8,
        std_input: *mut c_void,
        std_output: *mut c_void,
        std_error: *mut c_void,
    }
    #[repr(C)]
    struct ProcessInformation {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }
    unsafe extern "system" {
        fn CreateProcessW(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *const c_void,
            current_directory: *const u16,
            startup_info: *const StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const DETACHED_PROCESS: u32 = 0x8;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;

    let command_line = std::iter::once(executable.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|arg| quote_windows_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command_line = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let startup = StartupInfoW {
        cb: size_of::<StartupInfoW>() as u32,
        reserved: null_mut(),
        desktop: null_mut(),
        title: null_mut(),
        x: 0,
        y: 0,
        x_size: 0,
        y_size: 0,
        x_count_chars: 0,
        y_count_chars: 0,
        fill_attribute: 0,
        flags: 0,
        show_window: 0,
        reserved2_size: 0,
        reserved2: null_mut(),
        std_input: null_mut(),
        std_output: null_mut(),
        std_error: null_mut(),
    };
    let mut process = ProcessInformation {
        process: null_mut(),
        thread: null_mut(),
        process_id: 0,
        thread_id: 0,
    };
    // SAFETY: command_line 可写且以 NUL 结尾；其余指针为空或指向本函数内的有效结构体；
    // inherit_handles = 0，子进程不继承任何句柄。
    let created = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            null(),
            null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("无法启动 {}", executable.display()));
    }
    // SAFETY: 两个句柄由 CreateProcessW 返回，此后不再使用。
    unsafe {
        CloseHandle(process.thread);
        CloseHandle(process.process);
    }
    Ok(process.process_id)
}

#[cfg(not(windows))]
fn spawn_browser(executable: &Path, args: &[String]) -> Result<u32> {
    use std::process::{Command, Stdio};
    let child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("无法启动 {}", executable.display()))?;
    Ok(child.id())
}

/// 按 Windows 命令行解析规则给参数加引号（Chrome 和配置目录的路径里可能有空格）。
#[cfg(windows)]
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in arg.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // 引号前的反斜杠要加倍，再转义引号本身
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    // 结尾的反斜杠紧挨着收尾引号，同样要加倍
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(all(test, windows))]
mod tests {
    use super::{order_pages, quote_windows_arg};
    use serde_json::json;

    #[test]
    fn tab_order_survives_activation_and_close() {
        let ids = |pages: &[serde_json::Value]| {
            pages
                .iter()
                .map(|p| p["targetId"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let mut order = Vec::new();
        let mut pages = vec![json!({"targetId": "a"}), json!({"targetId": "b"})];
        order_pages(&mut order, &mut pages);
        assert_eq!(ids(&pages), ["a", "b"]);
        // 切到 b 后 Chrome 把 b 排到前面，还开了新页 c
        let mut pages = vec![
            json!({"targetId": "c"}),
            json!({"targetId": "b"}),
            json!({"targetId": "a"}),
        ];
        order_pages(&mut order, &mut pages);
        assert_eq!(ids(&pages), ["a", "b", "c"]);
        // 关掉 a
        let mut pages = vec![json!({"targetId": "c"}), json!({"targetId": "b"})];
        order_pages(&mut order, &mut pages);
        assert_eq!(ids(&pages), ["b", "c"]);
        assert_eq!(order, ["b", "c"]);
    }

    #[test]
    fn quotes_windows_args() {
        assert_eq!(quote_windows_arg("--no-first-run"), "--no-first-run");
        assert_eq!(
            quote_windows_arg(r"C:\Program Files\Google\chrome.exe"),
            r#""C:\Program Files\Google\chrome.exe""#
        );
        assert_eq!(
            quote_windows_arg(r"--user-data-dir=C:\a b\"),
            r#""--user-data-dir=C:\a b\\""#
        );
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_windows_arg(""), r#""""#);
    }
}
