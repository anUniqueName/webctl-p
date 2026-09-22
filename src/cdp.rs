use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::client_with_config, protocol::WebSocketConfig};

pub struct Cdp {
    socket: WebSocket<TcpStream>,
    next_id: u64,
    events: VecDeque<Value>,
    timeout: Duration,
    /// 页面弹出对话框时点"确定"（true）还是"取消"（false）
    pub accept_dialogs: bool,
    /// prompt 对话框的输入内容
    pub prompt_text: Option<String>,
    /// 本条命令执行期间处理过的对话框，由 main 附在输出里
    pub dialogs: Vec<Value>,
}

impl Cdp {
    pub fn connect(url: &str) -> Result<Self> {
        let (host, port) = ws_host_port(url)?;
        let address = (host.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("无法解析 CDP 地址"))?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))
            .with_context(|| format!("无法连接 {host}:{port}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(256 * 1024 * 1024))
            .max_frame_size(Some(256 * 1024 * 1024));
        let (socket, _) = client_with_config(url, stream, Some(config))
            .with_context(|| format!("WebSocket 握手失败：{url}"))?;
        Ok(Self {
            socket,
            next_id: 1,
            events: VecDeque::new(),
            timeout: Duration::from_secs(30),
            accept_dialogs: false,
            prompt_text: None,
            dialogs: Vec::new(),
        })
    }

    pub fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.call_timeout(method, params, session, self.timeout)
    }

    pub fn call_timeout(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.send(method, params, session.map(|session| json!(session)))?;
        // 和 wait_event 一样按绝对时限算：读超时设一次的话，每收到一个无关事件都等于重新给满一个
        // 超时窗口，事件不断的页面上这条命令永远等不到超时
        let deadline = Instant::now() + timeout;
        loop {
            self.set_read_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error)) if is_timeout(&error) => bail!(
                    "CDP 命令 {method} 超时：页面可能卡住或开着对话框；有界面时用 `webctl front` 切到前台查看"
                ),
                Err(error) => return Err(error.into()),
            };
            let Some(value) = parse_message(message)? else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    bail!("CDP 命令 {method} 失败：{error}");
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            if value.get("method").is_some() {
                self.on_event(value)?;
            }
        }
    }

    pub fn wait_event<F>(&mut self, mut predicate: F, timeout: Duration) -> Result<Option<Value>>
    where
        F: FnMut(&Value) -> bool,
    {
        if let Some(index) = self.events.iter().position(&mut predicate) {
            return Ok(self.events.remove(index));
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.set_read_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error)) if is_timeout(&error) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if let Some(value) = parse_message(message)?
                && value.get("method").is_some()
            {
                if predicate(&value) {
                    return Ok(Some(value));
                }
                self.on_event(value)?;
            }
        }
        Ok(None)
    }

    pub fn take_events<F>(&mut self, mut predicate: F) -> Vec<Value>
    where
        F: FnMut(&Value) -> bool,
    {
        let mut taken = Vec::new();
        let mut kept = VecDeque::new();
        while let Some(event) = self.events.pop_front() {
            if predicate(&event) {
                taken.push(event);
            } else {
                kept.push_back(event);
            }
        }
        self.events = kept;
        taken
    }

    fn send(&mut self, method: &str, params: Value, session: Option<Value>) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = session;
        }
        self.socket
            .send(Message::Text(request.to_string().into()))?;
        Ok(id)
    }

    /// 对话框弹出后页面脚本停住，页面相关的命令都等不到返回；而且只有对话框弹出时
    /// 开着 Page 域的会话才能关闭它，命令结束、会话断开后就再也关不掉了。
    /// 所以在所有读消息的地方统一按 accept_dialogs 当场处理，并记录下来。
    /// Page.handleJavaScriptDialog 的响应之后到达时 id 对不上，会被忽略。
    fn on_event(&mut self, event: Value) -> Result<()> {
        if event["method"] != "Page.javascriptDialogOpening" {
            self.events.push_back(event);
            return Ok(());
        }
        let mut params = json!({"accept": self.accept_dialogs});
        if let Some(text) = &self.prompt_text {
            params["promptText"] = json!(text);
        }
        self.send(
            "Page.handleJavaScriptDialog",
            params,
            event.get("sessionId").cloned(),
        )?;
        let handled = if self.accept_dialogs {
            "accept"
        } else {
            "dismiss"
        };
        self.dialogs.push(json!({
            "type": event["params"]["type"],
            "message": event["params"]["message"],
            "handled": handled
        }));
        Ok(())
    }

    fn set_read_timeout(&self, timeout: Duration) -> Result<()> {
        self.socket
            .get_ref()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        Ok(())
    }
}

/// read timeout 触发时 tungstenite 返回 WouldBlock 或 TimedOut，表示这段时间没数据，不是连接断开
fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn parse_message(message: Message) -> Result<Option<Value>> {
    match message {
        Message::Text(text) => Ok(Some(serde_json::from_str(&text)?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
        Message::Close(_) => bail!("Chrome 关闭了 CDP 连接"),
    }
}

fn ws_host_port(url: &str) -> Result<(String, u16)> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| anyhow!("只支持本机 ws:// CDP 地址"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("WebSocket 地址缺少端口：{url}"))?;
    Ok((host.trim_matches(['[', ']']).to_owned(), port.parse()?))
}
