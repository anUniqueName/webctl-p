# webctl 设计说明

## 目标

给 Claude Code、Codex 等 agent 使用的浏览器操作命令行工具。agent 通过执行命令操作 Chrome：打开网页、列出页面上可操作的元素（带编号）、按编号点击和输入、读取文字、截图、上传文件。每次操作后直接返回页面发生了什么变化，agent 不必每步都重新读整个页面。

参考过的项目和各自借鉴的部分：

| 项目 | 借鉴 |
|---|---|
| vercel-labs/agent-browser | Rust 命令行；快照给元素编号，按编号操作；点击前检查是否被遮挡；点击触发弹窗时不卡住 |
| browser-use | 只列出可见的可交互元素；点击走 CDP 鼠标事件（移动 → 按下 → 抬起） |
| GenericAgent | 操作后返回页面变化（跳转、新标签页、新出现的文字、弹窗）；可直接执行 JS |

## 不做的

- 不内置大模型，决策由调用它的 agent 做。
- 不内置识别验证码内容、自动解题的逻辑：不认字、不认图里的物体，不按验证类型自动选操作，也不自动把"算出来的位置"变成点击或拖动；`open` 等命令认出拦截页也只报告类型，不去处理。不改浏览器指纹。

  `gap` 是这条线内侧的：它只回答"这张图里被压暗的缺口在哪个坐标"这一个纯图像问题，返回候选坐标就结束，拖不拖、拖哪个候选、过没过都由调用方决定（同理 `find`/`vclick` 只做模板匹配）。

  提供的是通用输入操作，都要调用方显式调用：`turnstile`（量出 Cloudflare 控件在页面上的位置，点一下勾选框）、`clickat`（`--hold` 是长按）、`drag`、`move`、`find`/`vclick`。`move`、`clickat`、`vclick`、`drag` 的鼠标移动走拟人轨迹（见"视觉识别与坐标点击"一节）。遇到验证码或登录，默认由人完成：`front` 把窗口切到前台，人处理完再用 `wait` 等页面变化后继续；调用方的技能明确允许 agent 自己处理时，按调用方的规定用上面这些操作。点哪里、按多久、拖到哪都由调用方给出。

  不改指纹是因为不需要。点击走的是 CDP 的 `Input.dispatchMouseEvent`，页面收到的 `mousemove/mousedown/mouseup/click` 四个事件 `isTrusted` 都是 `true`，和人手点的没有区别，本身不带自动化标记（2026-09 在亚马逊上实测确认）。能被网站看出来的只有两样：开着的调试端口（网页读不到）和全新没有 cookie 的配置目录——被拦通常是后者，对策是让人登录一次把 cookie 攒上，不是伪装。

  同一次实测还澄清了一个容易误判的点：点击后返回里没有 `changes`，不等于点击没生效。当时是表单提交比默认 `settle` 长，等待加到 6 秒后正常提交并跳转。所以"没检测到变化"时输出会带一句提示，引导加长等待而不是改用 JS 合成点击。
- 不做 MCP（以后需要再加）。
- 不做常驻后台进程：每条命令独立连接 Chrome，执行完退出。

## 技术选型

- Rust 2024 edition，同步代码，不用 tokio。
- 只用 Cargo.toml 里已有的依赖：`tungstenite`（WebSocket，只连本机 `ws://`，不需要 TLS）、`serde`/`serde_json`、`clap`（derive）、`anyhow`、`base64`、`png`（`find`、`vclick` 解码截图和模板图）、`image`（只开 `jpeg`/`webp` 两个解码 feature，`gap` 的输入和 `find`/`vclick` 的模板图要认 JPEG/WebP）、`rusqlite`（命令日志；`screenshot` 输出的本地时间也借它算）。不为单个功能新增依赖——`image` 是 0.5.0 缺口识别和模板图格式扩展的共同需要，是唯一例外。
- 访问 `/json/version` 用 `std::net::TcpStream` 手写 HTTP/1.1 GET（带 `Connection: close`）。Chrome 调试端口不接受 HTTP/1.0，收到后直接断开连接；它的响应很简单，带 Content-Length。

## 文件布局

```
src/main.rs          命令行定义（clap）、命令分发、输出
src/cdp.rs           CDP 连接：发命令、等响应、收事件
src/browser.rs       启动/连接 Chrome、会话状态文件、标签页管理
src/page.rs          页面操作：快照、点击、输入、按键、滚动、等待、截图、视觉定位、变化检测
src/vision.rs        视觉识别：PNG 解码转灰度、两级 ZNCC 模板匹配
src/log.rs           命令日志：每条命令往 SQLite 追加一行
src/js/snapshot.js   注入页面的快照脚本（include_str!）
src/js/observe.js    操作前后的变化监测脚本（include_str!）
tests/fixture.html   冒烟测试用的本地页面
tests/smoke.rs       冒烟测试（需要本机有 Chrome，无头模式运行）
SKILL.md             给 Claude Code 的使用说明
README.md            中文说明
```

按键映射表等放在 page.rs 里，不再拆更多文件。

## 会话与状态

- 数据目录：环境变量 `WEBCTL_HOME`；未设置时 Windows 用 `%LOCALAPPDATA%\webctl`，其他系统用 `~/.webctl`。
- 全局参数 `--session <名字>`（默认 `default`，也可用环境变量 `WEBCTL_SESSION`）。每个会话一个状态文件 `<数据目录>/sessions/<名字>.json`：

  ```json
  {"endpoint": "http://127.0.0.1:9333", "launched": true, "current_target": "<targetId>", "tab_order": ["<targetId>"]}
  ```

  `endpoint` 可以是 `http://127.0.0.1:<端口>`（再从 `/json/version` 取 WebSocket 地址），也可以是直接的 `ws://...` 地址。
- Chrome 配置目录：`<数据目录>/profiles/<会话名>`。在里面登录一次后会一直保留，不影响用户日常使用的 Chrome。

## 连接浏览器（每条命令开头都走这一步）

1. 给了全局参数 `--cdp <端口|http地址|ws地址>`：连接它。地址和状态文件里记的是同一个时沿用已有状态（当前页、标签页顺序、`launched` 都保留）；状态文件读不出来或指向别的浏览器时新建一份（`launched: false`）。每条命令都带 `--cdp` 是常见写法，一律新建的话当前页每次都要重选，`launched` 还会被改成 false，`close` 随之不再关闭 webctl 自己启动的 Chrome。
2. 否则读状态文件；endpoint 可连通（`/json/version` 3 秒内返回）就直接用。超时用 3 秒和 `Cdp::connect` 一致：机器忙的时候 1 秒判不完，活着的浏览器会被当成连不上，白重启一次。
3. 否则启动 Chrome：
   - 可执行文件查找顺序：环境变量 `WEBCTL_CHROME` → Windows 常见路径（`%ProgramFiles%`、`%ProgramFiles(x86)%`、`%LOCALAPPDATA%` 下的 `Google\Chrome\Application\chrome.exe`）→ Edge（`Microsoft\Edge\Application\msedge.exe`，同样三个目录）→ macOS/Linux 常见路径。都找不到就报错，提示设置 `WEBCTL_CHROME`。三个目录里的 Chrome 全部找完才轮到 Edge：按目录逐个找的话，Chrome 装在 `%LOCALAPPDATA%` 的机器上会先命中 `%ProgramFiles%` 里的 Edge。
   - 端口：交给 Chrome 自己挑（`--remote-debugging-port=0`）。Chrome 启动后把实际端口写进配置目录的 `DevToolsActivePort` 文件，第一行就是端口，webctl 轮询这个文件再去连。之前是 webctl 自己 `bind 127.0.0.1:0` 拿一个空闲端口、放掉再传给 Chrome：Chrome 启动慢、或者两条命令同时第一次启动时会撞号，端口一直开不出来，命令失败也不写状态文件，而 Chrome 还活着占住配置目录，之后每条命令都重新启动一次，新进程把请求转交给已有实例就退出，端口永远等不到。
   - 启动参数：`--remote-debugging-port=0 --user-data-dir=<配置目录> --no-first-run --no-default-browser-check --hide-crash-restore-bubble`；全局参数 `--headless` 时再加 `--headless=new`。**不加** `--no-sandbox`，不加 `--enable-automation`，也不加任何用来隐藏自动化特征的参数。
   - 进程要脱离当前命令独立运行：stdin/stdout/stderr 设为 null；Windows 上用 `std::os::windows::process::CommandExt::creation_flags(0x8 | 0x200)`（DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP）。
   - Windows 上启动前把本进程的标准输入、输出、错误句柄设为不可继承。这些句柄通常是调用方（agent、测试程序）传进来的管道；被 Chrome 继承后，调用方要等 Chrome 退出才读到输出结束，命令看起来一直不返回。
   - 每 100ms 读一次 `DevToolsActivePort` 并试着连 `/json/version`，最多等 15 秒。启动前**不删**这个文件：同一个配置目录已经有 Chrome 在运行时（比如状态文件被删了），新进程把请求转交给已有实例后退出，这时文件里写的是那个实例的端口，照样能接上，不用先让人去关浏览器。文件是上次崩溃留下的旧端口时连不通，接着轮询等新进程覆盖它。
4. 用 `/json/version` 返回的 `webSocketDebuggerUrl` 建立**浏览器级**连接。页面操作先 `Target.attachToTarget {targetId, flatten: true}` 拿到 `sessionId`，之后页面命令都带上这个 `sessionId`，在同一个 WebSocket 上完成。
5. 选当前标签页：状态文件里的 `current_target` 仍存在（`Target.getTargets` 结果里 type=page）就用它；否则取第一个 type=page、URL 不以 `devtools://`、`chrome-extension://` 开头、也不是其他会话当前页的标签页；都没有就留空，等真要用页面的命令（`open`、`eval` 等，经 `attach_current`）再 `Target.createTarget {url: "about:blank"}`；`tabs`、`tab new`、`status` 不需要当前页，不新建。选定后写回状态文件。之前连上时一律新建：通道按 `--cdp … tab new` 初始化时，连接时建的空白页马上被 `tab new` 替下、没人再用，2026-09-19 七个通道收尾后剩下 2 个。"其他会话的当前页"指 `<数据目录>/sessions/` 下别的状态文件里、endpoint 和本会话相同的 `current_target`：多个会话（第二个起用 `--cdp`）共用一个 Chrome 时，直接取第一个标签页会选中别的会话正在用的页，之后的 `open` 会把它导航走（2026-09-19 的审计收尾时，3 个 `--cdp` 会话 `tab close` 后当前页都变成了主会话的标签页）。只有一个会话时没有要跳过的页，第一次连上照旧接管用户已经打开的标签页。按状态文件判断，会话不用了但状态文件还在时，它的当前页也会被跳过，下一条要用页面的命令会多开一个 about:blank；同一个 Chrome 用不同写法的地址连（`localhost` 和 `127.0.0.1`）时认不出是同一个。`tab close` 关掉当前页后，剩下的页都是其他会话的当前页时不新建 about:blank，`current_target` 留空（输出 `"current": null`），等这个会话下一条命令连接时按本步再选页或新建：多个会话收尾都只 `tab close`、之后不再使用时，新建的空白页会记在它们留下的状态文件里，之后的会话都会跳过这些页，主会话的 Chrome 常驻时标签页每轮都会变多（每轮 3 个会话时实测 1 → 4 → 7）。
6. 页面命令 attach 之后先 `Page.enable`，超时 5 秒。页面在 webctl 连上之前就弹出了对话框时，页面脚本停住，`Page.enable` 不会返回，这种对话框也无法通过 CDP 关闭（见"对话框"一节），此时报错，提示用 `webctl front` 切到前台由人处理。

## CDP 客户端（cdp.rs）

- `Cdp::connect(ws_url)`：先 `TcpStream::connect`，再用 `tungstenite::client` 握手；`WebSocketConfig` 把单条消息上限调到 256MB（整页截图可能很大）。
- `call(method, params, session_id: Option<&str>) -> Result<Value>`：发送 `{id, method, params, sessionId?}`，循环读消息：`id` 匹配就返回 `result`（有 `error` 时转成错误，带上 method 名和错误信息）；带 `method` 字段的是事件，存进内部队列。默认超时 30 秒，用底层 TcpStream 的 read timeout 实现。超时报错时提示页面可能卡住或开着对话框。`call_timeout` 可单独指定超时。
- 读消息时收到 `Page.javascriptDialogOpening`：立即按 `--on-dialog` 发送 `Page.handleJavaScriptDialog`，记录到 `dialogs`，然后继续等原来的响应（对话框关闭后 Chrome 才会回复）。所有命令都走这段逻辑，不需要单独的"带对话框检测"的调用。
- `wait_event(pred, timeout) -> Result<Option<Value>>`：先查队列，再继续读，直到超时。
- `take_events(pred) -> Vec<Value>`：取走队列里匹配的事件。
- read timeout 触发时 tungstenite 返回 `Io` 错误（WouldBlock 或 TimedOut），要当成"这段时间没数据"处理，不是连接断开。

## 元素定位与编号

### snapshot 脚本（js/snapshot.js）

- 先清掉页面上旧的 `data-webctl-ref` 属性，再从 `e1` 开始给元素编号，写到元素的 `data-webctl-ref` 属性上。编号存在页面里，所以命令之间不用保存状态。元素被页面重新渲染后编号会丢失，此时报"编号 eN 已失效，请重新 snapshot"。
- 遍历范围：主文档、同源 iframe 的 `contentDocument`、开放的 shadowRoot。跨域 iframe 不进入，只在输出里标一行 `[跨域 iframe] src=...`。
- 可交互元素：`a[href]`、`button`、`input`（type 不是 hidden）、`textarea`、`select`、`summary`、`[contenteditable]`（值为 true 或空）、role 为 button/link/checkbox/radio/tab/menuitem/menuitemcheckbox/menuitemradio/option/switch/combobox/textbox/searchbox/slider/treeitem 的元素、`tabindex>=0` 的元素，以及 computed style 为 `cursor: pointer` 且祖先里没有已入选元素的元素（用来覆盖 div 做的按钮）。
- 可见判断：宽高都大于 0，`display` 不是 none，`visibility` 不是 hidden，`opacity` 大于 0，祖先没有 `aria-hidden="true"`。不可见的不编号。
- 标签名取 `localName`（取不到时用 `nodeName`），不用 `tagName.toLowerCase()`：拦截页的脚本会改写 DOM 属性，实测 walmart.ca 上有元素的 tagName 取到 undefined，整个 snapshot 因此失败。每个元素的处理单独包 try/catch，读取出错就跳过，输出末尾加一行"N 个元素读取出错，已跳过"。
- 名称取值顺序：`aria-label` → `aria-labelledby` 指向元素的文字 → `<label for>` 或外层 label 的文字 → 可见文字（合并空白，截到 60 字）→ `placeholder` → `title` → `alt` → `value`（按钮类）。
- 类型显示：有 role 显示 role；否则 input 按 type 显示（text/search/email/password/number/tel/url 显示 textbox，其余如 checkbox、radio、file 显示原值）；其他显示小写标签名。
- 输出一行一个元素，例如 `[e3] button "登录"`，按需附带：`value="..."`（输入框，截 40 字；password 类型只显示长度）、`placeholder="..."`、`checked` / `unchecked`、`disabled`、`href=...`（截 80 字，同域链接只显示路径）、`selected="US" options=US,UK,DE…`（select，最多列 10 个）、`(视口外)`（不在当前视口内）。
- 开头三行：`url: ...`、`title: ...`、`scroll: <scrollY>/<scrollHeight-innerHeight>  viewport: <宽>x<高>`。
- `--in <TARGET>` 只处理该元素内部；元素超过 `--max`（默认 300）个时截断，最后一行说明总数和截断情况。

### 其他命令的目标参数 `<TARGET>`

- `e3` 或 `@e3`：按编号在主文档、同源 iframe、开放 shadowRoot 里查找。
- `text=文字`（也认 `text="文字"`、`text='文字'`）：在主文档、同源 iframe、开放 shadowRoot 里找整段文字（`innerText` 折叠空白后）恰好等于它的最内层元素，不做部分匹配。文字拆在几个子元素里（图标 + 文字、`<b>新增</b>物流渠道`）也认。只取可见的；同一段文字有多处时，先排除中心点在视口内、却被别的元素盖住的（弹窗后面的同名按钮、表格固定列下面那一份），还剩多个再优先可点击的（`cursor: pointer` 或在 button、a、`[role=tab]` 等里面）；最后仍有多个就报错并列出前 5 个，不替 agent 猜，免得点错行。视口外的元素判断不了是否被盖住，按候选保留。加这个是因为日志里 agent 约 230 次先用 eval 按文字找元素、打 `data-e2e` 属性、再用 CSS 选择器点它。
- 其他：当作 CSS 选择器，在主文档 `querySelectorAll`，取第一个可见的命中（可见判断同 `text=`）；都不可见时取第一个命中，隐藏的 `input[type=file]` 照样能 upload。同一个选择器命中好几个很常见（响应式页面桌面版、手机版各一份搜索框），只取第一个会选中隐藏的那个，fill 随之失败（eBay 店铺页的 `input[name="_bkw"]` 命中 3 个，第一个不可见）。`text`、`snapshot --in` 同样处理；`wait --selector`、`--gone` 看所有命中里有没有可见的。
- 找不到就报错；编号格式但找不到时提示重新 snapshot。

## 点击（click）

1. JS：找到元素 → `scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'})` → 取 `getBoundingClientRect()` 的中心点。元素在同源 iframe 里时，坐标加上各级 iframe 在上一级视口中的偏移（iframe 的 rect.left/top 加上 border 宽度和 padding）。`behavior: 'instant'` 不能省：页面设了 `scroll-behavior: smooth` 时滚动是动画，紧接着量到的是动画中途的位置，点击会落到别处。
2. 遮挡检查：用本地坐标在元素**自己的根**上调 `elementFromPoint`（`el.getRootNode()`，主文档里就是 document，开放 shadow root 里就是那个 shadow root）。命中的节点既不是该元素也不在该元素内部时，报错 `被 <div#consent.mask> 遮挡`（遮挡元素有编号就一并给出），加 `--force` 时跳过检查。不能用 `el.ownerDocument`：shadow root 里的元素在文档上取到的命中是宿主元素，`contains` 又不跨 shadow 边界，结果一律报被宿主遮挡，点不了。
3. CDP：依次发 `Input.dispatchMouseEvent`：`mouseMoved` → `mousePressed`（button=left，buttons=1，clickCount=1）→ `mouseReleased`，每步之间隔 30ms。坐标是顶层视口的 CSS 像素。`--right` 用 button=right、buttons=2；`--double` 在第一轮后再发一轮 press/release，clickCount=2。
4. 之后做"变化检测"（见下文）并输出。

## 视觉识别与坐标点击（find / vclick / move / clickat / drag）

- `find <IMAGE>`：`Page.captureScreenshot` 截当前视口 → `vision.rs` 解码成灰度图 → 两级 ZNCC 模板匹配（先按模板短边/8 降采样粗匹配，非极大值抑制后留至少 32 个候选、`--max` 更大时按 `--max` 留，回原图 ±(factor+1) 窗口精修；模板短边不到 16px 时倍数为 1，粗匹配就是在原图上做的，直接按阈值判定，不再精修一遍）。粗匹配只挑候选，**不按阈值筛**：目标左上角和降采样网格对不齐时粗分数会掉，掉多少取决于模板的细节有多细——42x25 的文字按钮（倍数 3）在 9 种对齐偏移下粗分数 0.67–1.0，24x16、笔画只有 1px 的模板（倍数 2）在 ox、oy 都是奇数的 16 个偏移上一个候选都出不来（细节被降采样平均掉了）。所以粗匹配把每个位置都算出来（只丢掉纯色窗口，它们的分数是负无穷），靠非极大值抑制取各邻域的最高分，再截前 N 名交给精修。代价是候选多了：1920x1080 的图上实测慢 0.7–3.5ms（页面样的底图）到 9–11ms（整张噪声图，最坏情况），比起截图和 PNG 解码可以忽略。倍数为 1 时没有降采样、也就没有对齐问题，粗匹配直接按 `--threshold` 筛。是否匹配由原图精修的分数按 `--threshold` 判定。窗口统计量走积分图，耗时跟模板大小基本无关。匹配分数 0–1，`--threshold` 默认 0.8；模板或窗口是纯色（没有方差）时分数记为负无穷，`--threshold` 给到 0 或负数也不会把纯色块当成匹配，`vclick` 不会照着点下去。截图是设备像素，输出坐标按视口宽高比换算回 CSS 像素，x/y 指匹配中心。**不做缩放不变性**：模板要和页面同一缩放、同一 devicePixelRatio，从 `webctl screenshot` 截的图里裁最稳。
- `vclick <IMAGE>`：find 取最佳匹配 → 拟人移动到中心 → 点击 → 变化检测。找不到匹配返回 `ok: false`，不盲点。
- `move <X> <Y>` / `clickat <X> <Y>`：目标直接用视口 CSS 坐标，不经过 DOM 定位（没有遮挡检查，遮挡本来就可能是目标的一部分，如 canvas、封闭 shadow root）。
- 拟人轨迹：二次贝塞尔曲线，控制点在中点垂线上随机偏 ±25% 距离；步数按距离 12–48 步，缓动先慢后快再慢，途中加亚像素抖动，步间 sleep 4–12ms，落点不抖。webctl 每条命令是新进程，鼠标位置按会话持久化在 `<数据目录>/sessions/<会话>.mouse`，轨迹从上次的落点出发。
- `clickat <X> <Y> --hold <毫秒>`：拟人轨迹移过去 → `mousePressed` → 保持指定毫秒 → `mouseReleased`，中间不移动。不带 `--hold` 时按下到松开隔 30ms，和之前一样。`--hold` 不能和 `--double` 同用。
- `drag <X1> <Y1> <X2> <Y2> [--duration <毫秒>]`：拟人轨迹移到起点（不按键）→ `mousePressed`（left，buttons=1）→ 停 100ms → 沿拟人轨迹移到终点 → 停 50ms → `mouseReleased`。移动途中每个 `mouseMoved` 都带 `button: "left"`、`buttons: 1`，页面收到的 mousemove 的 `buttons` 是 1，和人按着左键移动一样（测试页的滑块只认 `buttons === 1` 的移动）。`--duration` 是起点到终点的移动时长，默认 800ms：步数取"按距离算的步数"和 `duration/16` 中较大的，每步间隔 `duration/步数` 再随机乘 0.7–1.3。移动中途出错也先发 `mouseReleased`，免得页面一直以为左键按着。之后做变化检测。
- 这两个和 `clickat` 一样只按调用方给的坐标、时长执行，不看页面内容。加它们是因为 2026-09-19 的审计遇到长按验证和拖动拼图时 webctl 没有对应操作，agent 只能改用 curl 抓页面，数据质量随之下降；要不要拿它们处理验证由调用方的规定决定（见"不做的"）。
- `gap <背景图> [--piece <滑块图>] [--max N]`：纯本地图像计算，不连浏览器，只回答"被压暗的缺口在图里哪个坐标"。按 `vision.rs` 的实现，步骤是：
  1. 变暗量：用积分图算每个像素周围一个方窗的均值，再减去该像素的灰度值（负的算 0）。窗口半径取滑块长边的一半（给了 `--piece`），夹在 12–120 之间；**没给滑块图时不知道缺口多大，按 16、28、40 三个半径各算一遍**，三遍的检出一起进下面的合并。窗口要比缺口大，缺口内部才会整体显得比周围暗：固定 16（窗口 33x33）比常见的 48x52 缺口还小，缺口内部显不出整体变暗，只检出破碎的边缘，真缺口会被同一块干扰物的几段边缘挤出前 3 名（实测排第 4）。靠图片边缘的像素把窗口整体往里挪、不截短，窗口大小始终一致：截短会让边上的缺口只统计到一小块，包围盒被压小（实测 x 差 4、y 差 32）。
  2. 连通域：在 10/15/20/25/30 五个变暗量阈值下各取一次 4 邻接连通域（没给滑块图时是三个半径各五个阈值，共 15 遍），面积小于 300 或大于上限的丢掉。上限是滑块面积的两倍（给了 `--piece`），否则 8000。尺寸再按滑块筛一道：宽高都要落在滑块尺寸 ±25 内（下限不低于 15）；没有滑块图时用固定的 20–130。半径和面积上限都跟着滑块走，是因为真实验证码常给 2 倍、3 倍图（滑块 144×156），固定值下一个候选都出不来。
  3. 合并与打分：位置相差不到 12px 的检出算同一处，包围盒取并集（高阈值下的检出偏小），稳定性加一，IoU 和暗度取最大。分数 = 稳定性（有多少个"半径 × 阈值"的组合检出了它）+ IoU×4 + min(暗度, 60)/30；IoU 是连通域掩码最近邻缩放到滑块形状后和滑块 alpha 形状的交并比，没有滑块 alpha 时为 0。按分数从高到低返回最多 `--max` 个候选。
  4. 输出每个候选的 x/y/w/h、中心点 cx/cy 和各项打分依据。没给 `--piece` 时只剩暗度、稳定性和固定尺寸范围可用，候选常常不是真缺口，输出里写明：多半径只是让真缺口不再被自己的碎边缘挤掉，合成图里它能排第 1，但同样大小的纯方块干扰物紧跟其后、分数只差一点——没有形状可比就是分不出来。坐标是图片像素，换算成页面上的拖动距离要乘"显示宽度/图片宽度"，这一步和拖不拖、拖哪个候选一样由调用方决定。

## 输入

- `fill <TARGET> <TEXT>`：目标本身不是输入框、但在 `<label>` 里时，改为操作标签关联的输入框（`text=用户名` 找到的常是标签文字）。JS 聚焦元素，焦点没落到目标或它的子元素上就报错：不是输入框时接着输入会打进之前有焦点的那个框。报错写明原因：焦点被页面转到了别的元素时（点搜索框弹出另一个输入框）写出那个元素，如 `<input#real>`；目标不可见、已禁用时直说；其余情况说目标不是输入框。默认先全选已有内容——用 CDP 按键 `Control+A` 并带参数 `commands: ["selectAll"]`（跨平台可靠，对 React 受控输入框也有效）——再 `Input.insertText {text}`。TEXT 为空时全选后发一次 Backspace 删掉选中内容；`--append` 时不全选、用 JS 把光标移到末尾后插入，TEXT 又为空就什么都不做（这时没有选中内容，那一下 Backspace 会删掉原有内容的最后一个字）。完成后从 `document.activeElement` 读出当前值（contenteditable 读 innerText，目标在同源 iframe 或开放 shadow root 里时顺着 `contentDocument`/`shadowRoot` 的 activeElement 再往里找一层）放进输出，然后做变化检测。不重新定位一次目标来读：输入本身可能让目标失效（`text=` 找的是随内容变化的文字、页面重新渲染丢掉 `data-webctl-ref`），那时重新定位会报错，看起来像没填进去，重试一次又会填两遍。读不回来也不算失败，输出里 `value` 为 null。
- `type <TEXT>`：向当前获得焦点的元素逐字发键盘事件。ASCII 可打印字符用 `Input.dispatchKeyEvent`（keyDown 带 text/key/code/windowsVirtualKeyCode，然后 keyUp）；非 ASCII 字符（中文等）用 `Input.insertText` 逐字插入。`--delay <ms>` 设置字间隔，默认 0。
- `press <KEY>`：支持 `Enter Tab Escape Backspace Delete ArrowUp ArrowDown ArrowLeft ArrowRight Home End PageUp PageDown Space`、单个字母和数字，可加修饰键前缀 `Control+`、`Shift+`、`Alt+`、`Meta+`（如 `Control+A`、`Shift+Tab`）。修饰键位掩码：Alt=1、Control=2、Meta=4、Shift=8。
  - 产生文字的键（Enter 的 text 是 `"\r"`，Space 是 `" "`，没有 Control/Alt/Meta 修饰时的字母数字是字符本身）发 `keyDown`，其他发 `rawKeyDown`，然后发 `keyUp`。
  - 修饰键本身也要按顺序按下、最后抬起。
  - 之后做变化检测。
- `select <TARGET> <值或选项文字>`：只针对原生 `<select>`。先按 value 匹配，匹配不到再按选项文字匹配；设置后派发 `input` 和 `change` 事件（bubbles）。不是原生 select 就报错，提示"先 click 展开，再 snapshot 找到选项后点击"。之后做变化检测。
- `upload <TARGET> <文件>...`：先检查文件都存在；`Runtime.evaluate`（returnByValue=false）拿到元素的 objectId → `DOM.setFileInputFiles {files: [绝对路径...], objectId}`。元素不是 `input[type=file]` 时报错。

## 其他页面命令

- `open [URL] [--new-tab]`：URL 没有协议时补 `https://`（localhost、127.0.0.1 补 `http://`）；是本地已存在的文件路径时转成 `file:///` 地址（Windows 上不能用 `canonicalize`，它返回 `\\?\` 开头的路径）。`--new-tab` 先用 `Target.createTarget` 建空白页并设为当前标签页，然后和普通打开一样在当前标签页 `Page.navigate`；`Page.navigate` 返回 errorText 时报错。等加载完成：等 `Page.loadEventFired`，或轮询 `document.readyState === 'complete'`。`readyState` 变成 `interactive`（DOM 已就绪）后最多再等 3 秒：广告多的站 DOM 3–7 秒就好了，load 事件要等图片、广告、第三方脚本全部加载完，实测还要再晚 6–31 秒，而这时正文已经有最终字数的 94%–99%。没等到 load，或总时长超过 `--timeout`（默认 30000ms），都不算失败，输出里标 `"loaded": false` 并附提示：内容没出来就用 `wait` 等。`back`、`reload` 用同一套等待。输出 url、title、targetId。不带 URL 时只保证浏览器已启动。
- 拦截页识别（`open`、`back`、`reload`、`eval` 的输出）：执行一段页面 JS，认出拦截页或错误页时在输出里加 `"blocked": "<类型>"` 和按类型写的 `hint`。只是如实报告，命令照常成功；识别本身出错（比如页面正在跳转）时当作没认出。`eval` 也做识别，是因为很多拦截页是点筛选、翻页之后才跳出来的，agent 接着调的是 eval，不经过 open；脚本报错时同样带上。规则按下表顺序匹配，先中先返回：

  | 类型 | 特征 |
  |---|---|
  | `cloudflare` | `#challenge-form`、`#cf-challenge-running` 任一存在；或标题以 Just a moment / Attention Required / Checking your browser / 请稍候 开头；或页面里有勾选框控件（`.cf-turnstile`、`[id^="cf-chl-widget"]`、`iframe[src*="challenges.cloudflare.com"]` 任一存在），并且正文不到 5000 字、`[name="cf-turnstile-response"]` 还没有 token |
  | `perimeterx` | 有 `#px-captcha`；或标题以 Robot or human / Verify your identity 开头，并且 URL 路径含 `/blocked` 或正文含 Press & Hold |
  | `datadome` | iframe 或 script 的 src 含 `captcha-delivery.com` |
  | `ebay_interstitial` | URL 路径含 `/splashui/challenge` 或 `/splashui/captcha` |
  | `akamai` | 标题以 Access Denied 开头，并且正文含 `Reference #` |
  | `puzzle` | id 或 class 含 captcha、并且渲染出来了（`getClientRects()` 不为空）的元素（取前 20 个）自己的文字里有 Drag the puzzle、Drag the slider、Slide to complete、拖动、滑动 之一 |
  | `other` | 标题以 Robot or human、Access Denied、Pardon Our Interruption、Security Measure、Verify your identity、Challenge Validation、Security Check、Error Page 之一开头，并且正文不到 5000 字 |

  厂商专用的特征一条就算；标题这类通用特征必须再配一条，免得把站点自己的"Access Denied"权限页当成拦截页。Cloudflare 勾选框控件也算通用特征：评论表单、结账表单里嵌了 Turnstile 的普通商品页很常见，只凭控件就报拦截的话，有价格的页面会被记成拦截（实测 12KB 的商品页 eval 同时返回价格和 `blocked: "cloudflare"`），所以要正文很短、控件还没拿到 token 才算。5000 字是估的：拦截页、错误页只有一两句话，商品页、搜索页的正文远超这个数。正文只在标题或选择器先命中时才读，免得每次 open、eval 都读整页 innerText。puzzle 只看 captcha 元素自己的文字，不看整页：很多正常页面有 class 带 captcha 的 reCAPTCHA 角标；而且只看渲染出来的元素：`display:none` 的元素读 `innerText` 返回的是全部文字（HTML 规范对没渲染的元素这样规定），正常页面里预先放好、平时隐藏的滑块验证容器会被当成拼图页。先过滤再取前 20 个，隐藏元素不占名额。

  提示只写下一步有哪些选择，怎么处理由调用方按自己的规定决定，不写死"交给用户"：akamai 写站点拒绝访问、记为拦截；datadome 写先等验证 iframe 消失（设备检查页几秒后自己跳回），还在再记拦截；perimeterx、puzzle 写需要按住或拖动，是否用 `clickat --hold` / `drag` 或交人工由调用方决定；ebay_interstitial 写它有时几秒后自己跳回；other 写先截图看一眼。webctl 不写任何识别验证码内容、自动解题的逻辑。
- `turnstile [--timeout N]`：先 `Page.bringToFront`（后台标签页不渲染，控件量不出尺寸，会误报 widget_hidden）。然后每 250ms 用页面 JS 量一次控件位置，最少量 3 秒，`--timeout` 更长就量到 `--timeout`，最多 10 秒：先找 `iframe[src*="challenges.cloudflare.com"]`，再找 `.cf-turnstile`、`[id^="cf-chl-widget"]`，都看全部命中、取第一个有尺寸的。命中的是 0×0 的隐藏字段时（整页验证的主文档里常常只有 `cf-chl-widget-xxx_response`），量它的父元素，父元素一般就是挂封闭 shadow root 的容器，勾选框在里面；容器自己 0×0 时不量父元素，那是控件还没渲染出来，父元素可能是整个表单，点上去会误点别的东西。量到后在控件左侧 30px（控件宽不到 60px 时取中点）、垂直居中处发一次真实鼠标点击，只点一次、不重试，再每 500ms 判断一次是否通过，最多等 `--timeout`。通过的条件和定位用同一套：`cf-turnstile-response` 字段已有 token（内嵌控件过了页面不动），或者控件量不到、拦截页识别也认不出、`document.readyState` 不是 `loading`（整页验证过了会跳走）。只看拦截页识别的话，标题正常、控件在封闭 shadow root 里的页面还没点就会报通过；不看 `readyState` 的话，验证没过、页面自己重新加载换一道题时，新文档刚提交、标题和控件还没解析出来，也会报通过。点之前先按同一条件判断一次，已通过就不点。输出 `challenge`（widget_visible / widget_hidden / none）、`measured_from`（iframe / container）、`clicked`、`passed`、url、title。
- `snapshot [--in TARGET] [--max N]`：输出纯文本（格式见上文）。
- `text [TARGET] [--max N]`：输出元素或整个页面的 `innerText`，合并连续空行，默认截到 15000 字，截断时在末尾注明总长度。纯文本输出。
- `hover <TARGET>`：定位和遮挡检查同点击，只发 `mouseMoved`，之后做变化检测（悬停常会弹出菜单）。
- `scroll <up|down|top|bottom> [PX] [--in TARGET]`：up/down 在视口中心（或 `--in` 元素的中心）发 `mouseWheel`，deltaY 为 ±PX（默认 600）；top/bottom 用 JS `scrollTo`。等 300ms 后输出 scrollY。
- `eval <JS>` 或 `eval --file <路径>`：`Runtime.evaluate {expression, replMode: true, awaitPromise: true, userGesture: true}`。`replMode` 是 DevTools 控制台用的模式：同一页面上多次 eval 可以重复声明同名 `const`/`let`/`class`，顶层 `await` 直接返回结果；但它不等表达式返回的 Promise，所以结果是对象时再 `Runtime.callFunctionOn {functionDeclaration: "function () { return this; }", awaitPromise: true, returnByValue: true}` 取值。报"顶层 return"语法错误时再包成 `(async () => { ... })()` 按普通模式重试（不能按脚本是否含 "return" 字样判断）。REPL 模式只允许和之前 eval 里的声明重名，和页面自己的全局 `var`/`let`/`function` 重名仍报 `Identifier 'x' has already been declared`；报这个错时把脚本包进代码块 `{\n...\n}` 再按 REPL 模式执行一次：块里的 `const`/`let` 只在块内有效，最后一个表达式的值照样返回，顶层 `await` 照样能用。只在错误首行以 `SyntaxError: Identifier '` 开头、并且含 `has already been declared` 时重试：这是执行前的声明检查报出的语法错误，第一次一条语句都没执行，重试不会让脚本里的操作做两遍。不在任意错误文字里找 "has already been declared"：脚本执行到一半时，被 reject 的 Promise、自定义 Error 也可能带这段文字，那时重试会把前面的点击、提交再做一遍。仍然认不出的一种情况：脚本调用的页面函数在执行途中用 `eval` 抛出同样的 SyntaxError，这时也会重试，很少见，没有处理。脚本里用 `var` 和页面的 `let` 重名时包进块也没用（`var` 不受块限制），照原样报错。结果序列化成 JSON，超过 `--max`（默认 20000 字）截成字符串，输出加 `truncated: true`、完整长度 `length` 和提示（加大 `--max` 或先在页面里筛选）：截断后的字符串不能当 JSON 解析，之前 agent 按 JSON 解析失败才发现被截了。JS 抛异常时输出异常信息，`ok: false`。
- `screenshot [PATH] [--full] [--annotate]`：先 `Page.bringToFront`（后台标签页不产生画面，截图会一直等不到结果），再 `Page.captureScreenshot {format: "png"}`。`--full` 时用 `Page.getLayoutMetrics` 的 cssContentSize 作为 clip（旧版 Chrome 没有这一项，退回 contentSize，否则宽高是 null、截不出图），并设 `captureBeyondViewport: true`。`--annotate`：截图前往页面注入一个覆盖层，给带 `data-webctl-ref` 的可见元素画框并标上编号，截完移除。PATH 缺省时保存到 `<数据目录>/shots/<时间戳>.png`。输出绝对路径、图片宽高，以及截图时页面的 `url`、`title`（截图前用页面 JS 读 `location.href`、`document.title`，和 `open` 输出的一致）、`taken_at`（本地时间，ISO 8601 带时区，如 `2026-09-19T17:26:16+08:00`；标准库取不到本地时区，借已经依赖的 SQLite 算）、`overwrote`（目标文件原来已存在为 true）。已存在的文件照旧覆盖，只在输出里说明：2026-09-19 的审计里一个通道用同一个文件名把另一个通道的证据截图覆盖了，没人发现。不加 `--no-overwrite`：队列机器上装的是旧版，调用方一依赖新参数就会报错，文件名带上会话名和时分秒就能避免重名。
- `wait`：条件任选一个或组合使用（全部满足才返回）：`--selector CSS`（出现且可见；`script`、`style`、`template`、`meta`、`link`、`title`、`noscript` 从不渲染，按存在算，之前 `wait --selector '#__NEXT_DATA__'` 一律等满超时）、`--gone CSS`（不存在或不可见）、`--text 文字`（页面 innerText 包含）、`--url 子串`（URL 包含）、`--ms N`（单纯等待）。每 250ms 检查一次，`--timeout` 默认 30000ms。超时输出 `ok: false`。可以用来等人工完成验证码或登录。

  `--selector`、`--gone` 的"可见"和 `click`、`hover`、`fill` 定位时用的是同一段 JS（有尺寸、`display` 不是 none、`visibility` 不是 hidden、`opacity` 大于 0），源头只有一份常量。之前 `wait` 自己写了一份不看 `opacity` 的：元素还是 `opacity: 0`（淡入动画刚开始）时 `wait --selector` 就放行，紧接着的 `click` 又报"不可见"。`js/snapshot.js`、`js/observe.js` 各有自己的判定，不跟着改：snapshot 要跳过 `aria-hidden`，observe 故意不看 `opacity`，否则正在淡入的"保存成功"提示会被漏掉。
- `front`：`Target.activateTarget` + `Page.bringToFront`，把当前标签页和窗口切到前台。
- `tabs`：列出 type=page 的标签页：序号、targetId、url、title，当前标签页标 `"current": true`。`Target.getTargets` 按最近激活排序，直接用会让序号随切换变化；状态文件里的 `tab_order` 记录各 targetId 首次出现的先后，序号按它排，新标签页排在最后，已关闭的从记录里删掉。
- `tab <序号|targetId前缀>`：切换当前标签页（写状态文件并 activate）；`tab new [URL]`；`tab close [序号|targetId前缀]`（缺省关当前标签页；关的是别的页时当前页不变；关的是当前页时按"连接浏览器"第 5 步的规则重选，跳过其他会话的当前页，另外刚关的页会在 `Target.getTargets` 里短暂残留，也要排除；剩下的页都是其他会话的当前页时 `current` 为 null，不新建页；除了刚关的页一个标签页都不剩时才新建 about:blank，免得窗口因为没有标签页而关掉）。`close` 关的是整个浏览器，不是标签页。
- `back` / `reload`：执行 `history.back()` / `Page.reload`，先等主框架导航事件（单页应用是 `Page.navigatedWithinDocument`，最多等 5 秒），再等加载，输出 url、title。直接等加载会读到旧页面的 readyState。
- `open`、`back`、`reload` 在发起导航**之前**先把事件队列里本会话的 `Page.loadEventFired` 丢掉。webctl 接手页面时它可能还在加载（上一条命令返回 `"loaded": false`），attach、`Page.enable` 期间收到的是上一个文档的 load 事件；等加载那步先查队列，不丢的话立刻拿到旧事件，直接报 `"loaded": true`。只能在导航之前丢，之后丢会把新文档真正的 load 事件一起丢掉。
- 等加载时轮询 `document.readyState` 出错不算失败：跳转途中执行脚本会报 execution context destroyed，这时当作还没就绪接着轮询，不能让这个临时错误把整条 `open`/`back`/`reload` 带失败。
- 对话框没有单独的命令，见下文"对话框"一节。
- `status`：输出会话名、endpoint、能否连通、是否由 webctl 启动、标签页数量、当前标签页 url。会话不存在或连不上时直接返回 `reachable: false`，不启动浏览器（`close` 同理）。
- `close`：本会话的浏览器是 webctl 启动的就 `Browser.close`；是 `--cdp` 连接的只清空状态文件，不关用户的浏览器。

## 对话框

每条命令都是独立进程，命令结束后 CDP 会话随之断开。Chrome 只允许"对话框弹出时已开启 Page 域的会话"关闭它：下一条命令新建的会话关不掉上一条命令留下的对话框，而且对话框开着时页面脚本停住，`Page.enable` 都不会返回。所以对话框必须在弹出它的那条命令里当场处理：

- 全局参数 `--on-dialog accept|dismiss`（默认 dismiss，即取消）、`--prompt-text 文字`（prompt 的输入）。
- CDP 客户端读消息时收到 `Page.javascriptDialogOpening` 就按参数立即处理，并记录类型、文字和处理方式。
- 输出附 `dialogs` 数组；有 confirm/prompt/beforeunload 被取消时附提示：确认要执行时加 `--on-dialog accept` 重新操作。默认取消，避免误确认删除之类的操作。
- webctl 没在执行命令时页面自己弹出的对话框（如定时器触发）无法通过 CDP 关闭：下一条命令会在 `Page.enable` 5 秒超时后报错，提示用 `webctl front` 切到前台由人处理。

## 操作后的变化检测

适用于 click、press、hover、fill、select。借鉴 GenericAgent：每次操作后直接告诉 agent 页面发生了什么，省掉一次 snapshot。

1. 操作前：记录 `Target.getTargets` 里的 page 列表；执行 `js/observe.js` 的安装部分：记录 url、title、`document.body.innerText` 的长度，以及按行拆分后的行集合（只保留 2~80 字的行，最多 2000 行），存在 `window.__webctlObs` 里；同时启动 MutationObserver，把新增节点里的可见短文本（2~80 字）收集到 `window.__webctlObs.added`（最多 30 条），用来捕获"保存成功"这类很快消失的提示。
2. 执行操作。
3. 等页面稳定：至少等 300ms；之后每 100ms 读一次 url 和 innerText 长度，连续 300ms 不变就算稳定，最多等 `--settle`（默认 3000ms）。期间如果收到主框架导航事件（`Page.frameNavigated` 且 frame 没有 parentId），改为等 `Page.loadEventFired`（最多 `--timeout`）。
4. 期间弹出的对话框由 CDP 客户端按 `--on-dialog` 当场处理（见上一节），等待照常继续。页面跳转途中执行脚本报错时，当作"还在变化"继续等。
5. 操作后：执行 observe.js 的收集部分，对比得出：

   ```json
   {"ok": true,
    "changes": {"navigated": false, "url": "...", "title": "...",
                "new_tabs": [{"targetId": "...", "url": "..."}],
                "text_changed": true,
                "added": ["保存成功", "共 12 条"]},
    "hint": "页面内容有变化，需要时重新 snapshot"}
   ```

   `added` = MutationObserver 收集到的文字，加上操作后新出现的行（前后行集合做差），去重后最多 10 条。发生了页面跳转时 observe 数据已经丢失，只报 navigated、url、title。
6. `new_tabs`：操作前后 `Target.getTargets` 对比出的新 page。新标签页**不会**自动设为当前标签页，由 agent 用 `tab` 命令切换。

## 输出约定

- `snapshot`、`text` 输出纯文本；其他命令输出**一行 JSON**，成功时含 `"ok": true`。
- 失败时 stdout 输出 `{"ok": false, "error": "...", "hint": "..."}`（hint 可省略），退出码 1，同时往 stderr 写一行 `webctl: <错误>`：snapshot、text 的输出常接 `| grep`，只写 stdout 时错误行会被过滤掉，看起来像页面上没有要找的东西。错误信息用中文，写清原因和下一步怎么做。
- 输出一律 UTF-8，中文不转成 `\u` 转义（serde_json 默认行为即可）。

## 命令日志（log.rs）

每条命令执行完往 `<数据目录>/webctl.db`（SQLite，WAL）的 `commands` 表追加一行：`ts`、`session`、`command`、`argv`（JSON 数组）、`ok`、`ms`、`error`、`navigated`、`added_count`、`new_tabs`、`run_id`（环境变量 `WEBCTL_RUN_ID`）、`url`、`version`（`CARGO_PKG_VERSION`）、`hint`（输出里的 `hint`，没有就取 `note`——`gap`、`turnstile` 用的是这个键名）、`loaded`（`open`/`back`/`reload` 输出的 `loaded`）。写日志出错一律忽略，日志不能影响命令本身的结果；`WEBCTL_LOG=0` 关闭记录。老库靠一串 `ALTER TABLE ADD COLUMN` 补列，已有这列时报错忽略。

- `version`：收集来的日志跨多个版本，没有它一条失败记录对不上是哪个构建。
- 命令行解析失败也记一行：clap 解析不过时会直接结束进程，下面的 `log::record` 根本轮不到跑，agent 写错的调用——最该拿来改 SKILL.md 的信号——一点痕迹都不留。所以用 `try_get_matches`，真出错时（`--help`、`--version` 那两种正常输出除外）记一行 `ok = 0`、`error` 是 clap 报错的第一行，`command` 从 argv 里认（认不出记 `?`），会话名按环境变量和默认值取（`--session` 也没解析出来），然后照旧交给 `clap::Error::exit` 打印并退出，stderr 文字和退出码 2 都不变。
- 密码不进日志：`fill` 的目标、`type` 时当前有焦点的元素是 `input[type=password]` 时，命令在输出里多一个 `"masked": true`，main 据此把 argv 里那段文字（`fill`、`type` 的 TEXT 位置参数，按 clap 解析出的值逐个比对）换成 `***` 再记。stdout 的内容除了这个标记没有任何变化，`value` 照旧是真实内容。另外 `fill`、`type` 只要失败（目标不可见、已禁用、焦点被页面转走、连不上浏览器等），这段文字也一律换成 `***`：失败时还没判断出目标是不是密码框，而排查失败要看的是目标和错误原因，不是填进去的文字。**没覆盖的情况**：`eval` 的脚本是原样记的，脚本里写了密码就会留在库里。
- 日志只追加，不自动删除、不轮转。

## 已知限制

- 跨域 iframe 里的元素无法编号和点击（需要按 frame 单独 attach，后续再做）。
- webctl 没在执行命令时页面自己弹出的对话框（如定时器触发）无法通过 CDP 关闭，只能由人处理（见"对话框"一节）。
- 编号写在页面 DOM 属性上，页面脚本能看到这些属性。
- `drag` 发的是鼠标事件，测试只覆盖了监听 mousedown/mousemove/mouseup 的自定义滑块；HTML5 原生拖放（`draggable` 元素、`dragstart`/`drop` 事件）没有测过。

## 测试

- 单元测试（`cargo test --bin webctl`）：
  - browser.rs：标签页顺序不随激活变化、Windows 命令行参数加引号、`--cdp` 地址和状态文件相同时沿用已有状态（换地址或没有状态文件时新建）。
  - page.rs：按键解析（`Control+A`、`Shift+Tab`、`Enter`）、目标参数解析（`e3`/`@e3`/`text=`/CSS）、URL 补全、JPEG 尺寸解析、JS 异常取信息（`throw new Error` 取 description 第一行，`throw '文字'` 取 exception.value）。
  - vision.rs 模板匹配：原样嵌入能找到且接近满分、同一模板的两处都找到、随机模板不误匹配、纯色模板在 `--threshold` 0.5/0/-1 下都不匹配、整体提亮 40 后仍匹配、按钮样式的模板放在降采样网格的 9 种偏移上都能找到、笔画只有 1px 的 24x16 模板在 8x8 共 64 种偏移上都能找到（粗匹配不按阈值筛）、JPEG 解码。
  - vision.rs 缺口识别：合成缺口图有/无滑块形状都能检出、3 倍图上也能检出（半径和面积上限跟着滑块尺寸走）、没给滑块图时真缺口排进前 3（图里另有一块同样大小的方块干扰，固定半径 16 时真缺口排第 4）、全透明滑块图不 panic、纯噪声图不误报。
- `tests/smoke.rs`：找不到 Chrome 时跳过并打印原因。设置 `WEBCTL_HOME` 为 `target/smoke-home`，会话名 `smoke`，加 `--headless`，用编译好的二进制（`env!("CARGO_BIN_EXE_webctl")`）逐条执行命令，打开 `tests/fixture.html`，覆盖：
  1. snapshot 能看到输入框、按钮、链接、select、同源 iframe 里的按钮；
  2. fill 输入中文后 value 正确；
  3. click 按钮后 `added` 里出现 fixture 弹出的提示文字（该提示 1 秒后自动消失）；
  4. 点击被遮罩挡住的按钮时报遮挡错误；
  5. select 选择选项后值正确；
  6. 点击 `target=_blank` 的链接后 `new_tabs` 非空；
  7. 点击会弹出 `confirm()` 的按钮：默认按取消处理并记录在 `dialogs` 里，页面显示取消结果；加 `--on-dialog accept` 后页面显示确认结果；`eval` 里弹出的 alert 同样当场处理；
  8. upload 上传 `tests/fixture.html` 自身后，页面显示文件名；
  9. 点击同源 iframe 里的按钮生效；
  10. `screenshot --annotate` 生成 PNG 文件；
  11. `snapshot --in` 只输出范围内的元素；`eval` 结果超过 `--max` 时 `truncated` 为 true、`length` 是完整长度；`wait --selector script` 不因 script 不渲染而超时；`eval` 支持 `return` 写法，也能返回对象；同一页面重复声明同名 `const` 不报错，顶层 `await` 和表达式返回的 Promise 都拿到结果；
  12. `click text=文字`：被遮罩盖住的同名按钮不选；文字拆在子元素里也找得到；同名的两个按钮都能点时报错；找不到时报错；同源 iframe 里的按钮也点得到；
  13. 从截图裁出按钮当模板，`find`/`clickat`/`vclick` 能找到并点中（fixture 上方有 4 个同尺寸的相似按钮，按钮坐标也没对齐降采样网格）；
  14. 最后 `close`，之后 `status` 返回 `reachable: false`，不会重新启动浏览器；
  15. 页面脚本改写了 DOM 属性：tagName 为 undefined 的元素照样列出，读 localName 就抛错的元素被跳过并在末尾报个数；`eval` 里 `const` 和页面自己的全局变量重名也能执行；失败的命令 stderr 有 `webctl: ` 开头的一行；
  16. 选择器命中多个时 fill 选中可见的那个，`wait --selector` 也认；fill 报错写明焦点被转到哪个元素、目标已禁用、目标不可见；隐藏的 `input[type=file]` 照样能 upload；
  17. 拦截页识别：`tests/blocked/` 下 PerimeterX、Akamai、DataDome、拼图、eBay 过渡页（放在 `splashui/challenge.html`，让地址带 `/splashui/challenge`）、其他拦截页各一个最小的静态页面，`open` 报出对应类型，提示不含"交给用户"；`eval`（包括脚本报错时）、`back`、`reload` 的输出也带 blocked；标题是 Access Denied 但正文很长的页面不报；正文很长的页面里嵌了 `.cf-turnstile` 不报；fixture 里 `display:none` 的 captcha 容器写着"拖动"，正常页面照样不报；
  18. `turnstile`：内嵌控件靠 token 判定通过，已通过的不再点；控件在封闭 shadow root 里时量容器；`tests/turnstile-managed.html` 标题正常，主文档里只有 0×0 的隐藏字段，勾选框 iframe 在它父元素的封闭 shadow root 里、4 秒后才插入，turnstile 要量父元素、量得够久、先点了才报通过；内嵌控件拿到 token 后 `eval` 不再报 blocked；没有控件的验证页报 widget_hidden，不点；
  19. `screenshot` 输出带 url、title、taken_at，第一次写某个路径 `overwrote` 为 false，同一路径再截一次为 true；
  20. 两个会话共用一个 Chrome（`smoke2` 用 `--cdp` 连 smoke 启动的无头 Chrome 并 `tab new`）：smoke 关掉别的页时当前页不变，关掉自己的当前页后剩下的只有 smoke2 的页，`current` 为 null，不接管也不新建；第三个会话 `smoke3` 用 `--cdp` 第一次连上时，当前页不是 smoke2 的；再模拟一个通道 `lane1`：`--cdp` 连上后 open、`tab close`，`current` 为 null，收尾后标签页数不增加；通道 `lane2` 按 concurrency 的初始化写法 `--cdp … tab new` 再 `tab close`，标签页数也不增加（连接时不能先为它建空白页）；
  21. `clickat --hold` 和 `drag`：fixture 里一个按钮要按住不少于 1500ms 才标记完成，一个自定义滑块要按住左键拖到最右才标记完成（测试页面不模仿任何真实验证码）。普通 `clickat` 不算按住；`clickat --hold 1600` 完成；`drag` 从滑块中心拖到轨道右端完成，页面收到了 `buttons` 为 1 的 mousemove；页面收到的鼠标事件 `isTrusted` 都是 true。
  22. 开放 shadow root 里的按钮点得到（遮挡检查在元素自己的根上取命中元素，不会把宿主当成遮挡物）；`fill` 的目标是一段"填完文字就会变"的标签时值照样读得回来（从当前焦点读，不重新定位目标）；`fill <目标> "" --append` 一个字都不删。
  23. `wait --selector` 和 `click` 的可见判定一致：fixture 里一个 `opacity: 0` 的按钮 600ms 后才显出来，`wait` 不能早于 600ms 返回，返回后 `click` 要立刻点得到。
  24. 密码不进日志：往 `input[type=password]` 里 `fill`、`type` 之后，stdout 的 `value` 照旧是真实内容、多一个 `masked`，而 `webctl.db` 里这两行的 argv 是 `***`，整张表里搜不到明文。
- `logs_commands_to_sqlite`（不需要 Chrome）：`status` 和连不上的 `click` 各记一行，`ok`、`error`、`argv`、`run_id` 正确；`WEBCTL_LOG=0` 时不写；每行的 `version` 是当前版本；命令行写错（`status --nope`）也记一行 `ok = 0`、`command` 是 `status`、`error` 里有 `--nope`，退出码仍是 2、报错仍写 stderr；`--help` 不记；失败的 `fill`（连不上的端口）argv 里目标照记、文字记成 `***`，整张表搜不到那段文字。
- `cargo build`、`cargo clippy -- -D warnings`、`cargo test` 全部通过。

## 文档

- `SKILL.md`：给 Claude Code 的技能说明。front matter 含 `name: webctl` 和 `description`（写明什么时候使用）。正文包括：基本流程（open → snapshot → click/fill → 看返回的变化 → 必要时重新 snapshot）、每个命令一句话说明、常用写法（open 之后用 wait 等、多行 JS 用 `eval --file`、screenshot 的路径是位置参数、看大图用 `open --new-tab` 再 `tab close`、多个会话共用一个 Chrome 时哪些命令会切前台）、遇到登录或人机验证时的处理（默认 front → 请用户处理 → wait；调用方的技能明确允许时按调用方的规定用通用输入操作）、`blocked` 各类型的含义、注意事项（新标签页不会自动切换、对话框默认按取消处理、页面重新渲染后编号失效）。不超过 160 行。
- `README.md`：中文。安装（`cargo install --path .`，再把 SKILL.md 同步到 `~/.claude/skills/webctl/`，否则 agent 读到的是旧版说明）、命令列表、安全说明：调试端口在本机可被任何程序连接；配置目录里保存着登录 cookie；`eval` 可以在页面里执行任意 JS。
