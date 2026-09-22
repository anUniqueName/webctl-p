# 更新记录

## 0.6.0 - 2026-09-22

- 启动 Chrome 改为让 Chrome 自己挑调试端口（`--remote-debugging-port=0`），再从配置目录的 `DevToolsActivePort` 读回来。之前是 webctl 自己占一个空闲端口再放掉、传给 Chrome：Chrome 启动慢或两条命令同时首次启动时会撞号，端口开不出来，命令报错还不写状态文件，而 Chrome 已经活着占住了配置目录，之后每条命令都重新启动一次、每次都等满 15 秒超时。现在同一个配置目录已有 Chrome 在跑时也能接上它，不用先让人去关浏览器。
- `click`、`hover`、`scroll --in` 定位元素时滚动改为不带动画（`behavior: 'instant'`）。页面设了 `scroll-behavior: smooth` 时，之前量到的是滚动动画中途的位置，点击落在别处。
- 开放 shadow root 里的元素现在点得到。遮挡检查改为在元素自己的根上取命中元素，之前用文档取，拿到的是宿主元素，一律报"被 … 遮挡"。
- `fill` 填完后改从当前焦点读回值，不再重新定位一次目标。目标是随内容变化的文字（`text=`）、或者页面重新渲染丢掉了编号时，之前会报"找不到元素"，看起来像没填进去，重试一遍就填了两次。读不回来也不再让命令失败。
- `fill <目标> "" --append` 不再删掉最后一个字：这条命令本意是什么都不改，之前会发一次 Backspace。
- `open`、`back`、`reload` 不再把上一个文档的 load 事件当成本次的：发起导航前先把队列里本会话的 `Page.loadEventFired` 丢掉，之前页面接手时还在加载的话会立刻报 `"loaded": true`。跳转途中读 `document.readyState` 报错也不再让整条命令失败，改为接着等。
- `eval` 里 `throw '文字'` 的错误信息不再只剩一个 `Uncaught`：CDP 把这种异常的内容放在 `exception.value`，现在会取出来。
- 每条命令都带 `--cdp` 时不再丢掉会话状态：地址和状态文件里记的是同一个浏览器时沿用已有状态，当前页和标签页顺序都保留，`launched` 也不会被改成 false（改成 false 后 `close` 就不再关闭 webctl 自己启动的 Chrome）。
- 判断浏览器是否还在的 HTTP 超时从 1 秒放到 3 秒（和 CDP 连接一致）：机器忙的时候活着的浏览器会被当成连不上，白重启一次。
- CDP 命令超时改按绝对时限算。之前读超时只设一次，每收到一个无关事件就等于重新给满一个超时窗口，事件不断的页面上命令永远等不到超时。
- 输出改用不会 panic 的写法：`webctl text | head` 这类用法里读的一方先退出、管道断了，之前 `println!` 会 panic，退出码变成 101。
- Windows 上查找浏览器改为先找完三个常见目录里的 Chrome 再找 Edge，和 DESIGN.md 写的顺序一致。之前按目录逐个找，Chrome 装在 `%LOCALAPPDATA%` 的机器上会先命中 `%ProgramFiles%` 里的 Edge。
- `screenshot --full` 在旧版 Chrome 上（`Page.getLayoutMetrics` 没有 `cssContentSize`）退回用 `contentSize`，之前宽高是 null、截不出图。
- `find`/`vclick`：纯色（没有方差）的模板或窗口分数记为负无穷，`--threshold` 给 0 或负数时不再返回一堆假匹配，`vclick` 也就不会照着点下去；`--max` 大于 64 时候选名额跟着 `--max` 走，之前超出的部分被默默砍掉；模板短边不到 16px 时少扫一遍原图（粗匹配本来就是在原图上做的），相关性累加改用整数，大模板上不再丢精度。
- `gap`：局部均值半径和连通域面积上限跟着 `--piece` 给的滑块尺寸走。之前是固定值，2 倍、3 倍图（滑块 144×156）一个候选都出不来，只能要求用没缩放的原图。靠近图片边缘的缺口包围盒也更准了（局部均值窗口靠边时整体往里挪，不再截短）。没给 `--piece` 时输出里写明候选不可靠。
- `find`/`vclick`：粗匹配不再按"阈值减 0.4"筛候选，改成全部算出来、靠非极大值抑制取各邻域最高分再截前 N 名。笔画只有 1px 的小模板（24x16）在降采样相位对不齐的 16 种偏移上之前一个候选都出不来，直接报找不到。1920x1080 的图上慢 0.7–3.5ms（页面样的底图）到 9–11ms（整张噪声图，最坏情况）。
- `gap` 不带 `--piece` 时按 16、28、40 三个半径各算一遍再合并。之前固定半径 16（窗口 33x33）比常见的 48x52 缺口还小，缺口内部显不出整体变暗、只检出破碎的边缘，真缺口会被同一块干扰物的几段边缘挤到第 4 名、落在默认 `--max 3` 之外。
- `wait --selector`、`wait --gone` 的可见判定改成和 `click`、`fill` 同一份（多看 `opacity > 0`）。之前元素还是 `opacity: 0`（淡入动画刚开始）时 `wait` 就放行，紧接着的 `click` 又报"不可见"。
- 命令日志新增 `version`（产生这行的 webctl 版本）、`hint`（给 agent 的提示，`gap`/`turnstile` 的 `note` 也记在这里）、`loaded` 三列。
- 命令行写错（clap 解析不过）现在也记一行日志：之前 clap 直接结束进程，agent 写错的调用一点痕迹都不留。stderr 的报错文字和退出码 2 都不变，`--help`、`--version` 不记。
- 往密码框里 `fill`、`type` 时，日志的 `argv` 里那段文字记成 `***`（命令输出多一个 `"masked": true`，stdout 的 `value` 照旧）。`eval` 的脚本一律原样记录。
- `fill`、`type` 失败时（目标不可见、已禁用、焦点被页面转走、连不上浏览器等），日志的 `argv` 里那段文字也记成 `***`：这时还判断不出目标是不是密码框，之前密码会原样留在库里；排查失败要看的是目标和错误原因，不是填进去的文字。
- README 增加"收集日志"：库是 WAL 模式，只拷 `webctl.db` 会漏掉最近的行，要么三个文件一起拷，要么用 `VACUUM INTO` 取一致副本；日志只追加，不自动清理。
- 会话状态文件去掉没人读的 `pid` 字段（旧文件照常能读）。
- 发版流程：GitHub Actions 跑测试前先确认 runner 上装着 Chrome。冒烟测试找不到 Chrome 时会跳过并照常通过，等于没测过就发版。

## 0.5.0 - 2026-09-20

- 新增 `gap BG [--piece PIECE] [--max N]`：滑块验证码缺口识别，纯本地计算、不需要浏览器、不需要训练模型。思路来自验证码场景的常见结构——缺口是一块被压暗、边缘带亮边的区域：先算每个像素相对大窗口（窗口比缺口大，缺口内部才整体偏暗）局部均值的变暗量，在 10/15/20/25/30 五个阈值下取连通域，跨阈值合并（包围盒取并集），按"跨阈值稳定性 + 形状 IoU×4 + 暗度（封顶 60）"打分；给 `--piece` 时用滑块图 alpha 通道抠出形状算 IoU，区分多缺口干扰。候选按分数排序全返回，第一个拖不过就试下一个。滑块图没有 alpha（JPEG 等）时形状打分不生效，输出里写明。实测两组真实顶象滑块图（linux.do 帖子附件），正确缺口都在候选前两名。
- `find`/`vclick` 的模板图和 `gap` 的输入图从只认 PNG 扩展到 PNG/JPEG/WebP：PNG 仍走内置 `png` 解码器，JPEG/WebP 走新增的 `image` crate（只开 `jpeg`/`webp` 解码 feature）。DESIGN.md 的依赖清单相应更新。
- 测试：`gap` 的合成图用例（有/无滑块形状、全透明滑块不 panic、纯噪声不误报）、JPEG 解码用例。

## 0.4.1 - 2026-09-20

- 多个会话共用一个 Chrome 时不再留下没人用的空白页。`--cdp` 第一次连上、没有可选的页时，之前连接时就新建 about:blank 当当前页；通道按 concurrency 的写法 `--cdp … tab new` 初始化时，这个空白页马上被 `tab new` 替下，没人再用。2026-09-19 七个通道并行审计，收尾后剩下 2 个。现在连接时当前页留空，等真要用页面的命令（`open`、`eval`、`screenshot` 等）再新建；`tabs`、`tab new`、`status` 不新建。`front` 在当前页为空时也会新建，不再报"没有当前标签页"。
- `wait --selector`：`script`、`style`、`template`、`meta`、`link`、`title`、`noscript` 这类从不渲染的元素按存在算。之前一律判不可见，`wait --selector '#__NEXT_DATA__'`（Walmart）每次等满超时。`--gone` 同样按存在算。
- `eval`：结果超过 `--max` 被截断时，输出加上完整长度 `length` 和提示（加大 `--max` 或先在页面里筛选）。截断后的 `result` 是字符串，不能当 JSON 解析；之前只标 `truncated`，agent 按 JSON 解析失败才发现（Google Lens 同图页 400 条外链）。
- DataDome 的提示改为先等验证 iframe 消失再判断：Etsy 首页出现的是设备检查页，几秒后自己跳回原页（地址带 `dd_referrer`），之前的提示写"站点拒绝访问、记为拦截"。`blocked` 的取值不变。SKILL.md 的拦截类型表把 `akamai`、`datadome` 分成两行。
- 发布：推 `v*` 标签时 GitHub Actions 在 Windows 上跑测试、编译，把 `webctl.exe` 和 SKILL.md 等打成 `webctl-windows-x86_64.zip` 发到 Releases，发布说明取 CHANGELOG 里这个版本的一节。装机可以直接下载 exe，不用装 Rust。README 加了“发布”一节。

## 0.4.0 - 2026-09-19

- `eval`：`const`/`let` 和页面自己的全局变量重名时不再报 `Identifier 'x' has already been declared`。报这个错时把脚本包进代码块 `{ ... }` 再执行一次，返回值和顶层 `await` 不受影响；平时不包，之前 eval 里声明的变量之后照样能用。只在错误首行是 `SyntaxError: Identifier '…' has already been declared` 时重试：这个错误是执行前的声明检查报出的，第一次一条语句都没执行；脚本执行到一半时 reject 的 Promise、自定义 Error 里带这段文字的不重试，免得前面的点击、提交做两遍。
- `snapshot`：标签名改用 `localName`。拦截页的脚本改写过 DOM 属性时（walmart.ca 上有元素的 tagName 取到 undefined），之前整个快照报 `Cannot read properties of undefined (reading 'toLowerCase')`；现在单个元素读取出错就跳过，末尾报"N 个元素读取出错，已跳过"。`click` 的遮挡说明、`text=` 的候选说明同样改用 `localName`。
- 命令失败时除了 stdout 的 `{"ok": false, ...}`，再往 stderr 写一行 `webctl: <错误>`。stdout 格式不变；`snapshot | grep` 这类用法之前会把错误行过滤掉，只剩一个看不见的退出码。
- CSS 选择器命中多个元素时先取第一个可见的，都不可见才取第一个（隐藏的 `input[type=file]` 照样能 upload）。之前一律取第一个，eBay 店铺页 `fill 'input[name="_bkw"]'` 选中的是隐藏的那个。所有带目标参数的命令都生效，`text`、`snapshot --in` 也是；`wait --selector`/`--gone` 改为看所有命中里有没有可见的。
- `fill` 拿不到焦点时写明原因：焦点被页面转到了别的元素（写出那个元素，如 `<input#real>`）、目标不可见、目标已禁用，或者不是输入框。之前一律报"不是输入框"。
- 拦截页识别：之前只认 Cloudflare，Walmart（PerimeterX）、Kohl's（Akamai）、Etsy（DataDome）、eBay 验证过渡页和错误页、TikTok 拼图等拦截页打开后照常返回，没有 `blocked` 字段，被当成 0 结果。现在 `blocked` 的取值有 `cloudflare`、`perimeterx`、`akamai`、`datadome`、`ebay_interstitial`、`puzzle`、`other`。厂商专用的特征（`#px-captcha`、`captcha-delivery.com`、eBay 的 `/splashui/` 路径）一条就算；Access Denied、Error Page 这类通用标题要再配一个特征（Akamai 要正文有 `Reference #`，其余要正文不到 5000 字），免得把站点自己的权限页当成拦截页。Cloudflare 勾选框控件（`.cf-turnstile`、`[id^="cf-chl-widget"]`、`challenges.cloudflare.com` 的 iframe）也按通用特征处理，要正文不到 5000 字、`cf-turnstile-response` 还没有 token 才算：评论、结账表单里嵌了 Turnstile 的普通商品页不报（之前 12KB 的商品页 eval 同时返回价格和 `blocked: "cloudflare"`）；`#challenge-form`、`#cf-challenge-running` 和 Just a moment 一类标题仍然一条就算。拼图规则只看渲染出来的 captcha 元素：`display:none` 的元素读 `innerText` 返回全部文字，正常商品页里预先放好、平时隐藏的滑块容器之前会让 `open` 报 `puzzle`。`back`、`reload`、`eval` 之前完全不识别，现在输出也带 `blocked`；`eval` 脚本报错时也带，拦截页上读不到数据往往就是这个原因。
- 拦截页的提示按类型写，不再写死"交给用户"：akamai、datadome 写站点拒绝访问、记为拦截；perimeterx、puzzle 写需要按住或拖动，是否用 `clickat --hold` / `drag` 或交人工由调用方按自己的规定决定。Cloudflare 的提示不再建议 `wait --gone "#challenge-form"`：只靠标题认出来的验证页上本来就没有这个元素，会立刻返回。
- `screenshot`：输出加上截图时页面的 `url`、`title`、`taken_at`（本地时间，如 `2026-09-19T17:26:16+08:00`），以及 `overwrote`（目标文件原来已存在时为 true）。2026-09-19 的审计里有一张证据图拍到的是拦截页，另一张被别的通道用同名文件覆盖，都没人发现。已存在的文件照旧覆盖，没有加 `--no-overwrite`。
- 多个会话共用一个 Chrome 时，`tab close` 关掉当前页后、或者当前页找不到时（包括 `--cdp` 第一次连上），重选当前页会跳过其他会话正在用的页（按 `sessions/` 下 endpoint 相同的其他状态文件里的 `current_target` 判断）。`tab close` 后剩下的页都是其他会话的当前页时，不新建 about:blank，输出 `"current": null`，这个会话下一条命令连接时再选页或新建；只在除了刚关的页一个标签页都不剩时才新建 about:blank。通道每轮换新会话名、收尾只 `tab close` 时，新建的空白页会记在不再使用的状态文件里，之后的会话都跳过它们，主会话的 Chrome 常驻时标签页每轮都变多（每轮 3 个会话实测 1 → 4 → 7）。`--cdp` 第一次连上、当前页找不到时，没有可选的页照旧新建 about:blank。之前取标签页列表里的第一个，2026-09-19 的审计收尾时 3 个 `--cdp` 会话 `tab close` 后当前页都成了主会话的标签页，接下来的 `open` 会把主会话的页面导航走。只有一个会话时行为不变。另外 `tab close <序号>` 关的是别的页时，当前页不再跟着变。
- `turnstile`：整页 Cloudflare 验证之前一律报 `widget_hidden`，一次都没点上（2026-09-19 的审计里 9 次调用全是这样，截图后手动 `clickat` 每次都能过）。现在量位置之前先把标签页切到前台（后台标签页不渲染，量不出尺寸）；选择器的全部命中都看，取第一个有尺寸的，命中的是 0×0 的隐藏字段 `cf-chl-widget-xxx_response` 时量它的父元素（挂封闭 shadow root 的容器）；量的时间从固定 3 秒改为跟 `--timeout`，最少 3 秒、最多 10 秒。判断"已通过"改用和定位同一套条件：token 已写入，或者控件量不到、页面也认不出是拦截页，并且文档不在解析中（`readyState` 不是 `loading`）。之前只看拦截页识别，标题正常的验证页会还没点就报 `passed: true`；不看 `readyState` 的话，验证没过、页面自己重新加载换一道题时，新文档刚提交、标题和控件还没解析出来，也会报通过。输出字段和取值不变。`open` 的 Cloudflare 识别相应加上 `[id^="cf-chl-widget"]`（和 `.cf-turnstile` 一样要正文很短才算，见上面拦截页识别一条）。
- 新增长按和拖动。`clickat X Y --hold <毫秒>`：拟人轨迹移过去，按下，保持指定毫秒，再松开；不带 `--hold` 时和之前一样，`--hold` 不能和 `--double` 同用。`drag X1 Y1 X2 Y2 [--duration <毫秒>]`：拟人轨迹移到起点，按下左键，按住左键沿拟人轨迹移到终点（移动事件带 `buttons=1`，默认 800ms 拖完），松开。2026-09-19 的审计遇到长按验证（Walmart）和拖动拼图（TikTok Shop）时没有对应操作，agent 翻了几次帮助没找到，只好改用 curl 抓页面。这两个都是通用输入操作，只按给定的坐标和时长执行，webctl 不识别验证码内容、不自动解题；拿它们处理验证与否由调用方的规定决定。
- `--help`：`open`、`text`、`eval`、`screenshot`、`wait`、`clickat`、`drag` 补上说明，写明 `screenshot` 的路径是位置参数（没有 `--path`、`-o`）、`eval` 多行脚本用 `--file` 和 `--max` 的作用、`text` 可以只读某个元素、`open` 之后用 `wait` 等内容而不是固定 sleep。之前这些命令的帮助是空的，agent 每次开工都要试错。
- 文档改成和实际行为一致：
  - SKILL.md：去掉"不要尝试绕过验证码"，改为如实说明：webctl 提供 `turnstile`、`clickat`、`clickat --hold`、`drag`、`find`/`vclick` 这些通用输入操作，不内置识别或自动解验证码的逻辑；遇到验证页默认请用户处理，调用方的技能明确允许时按调用方的规定做。补上 `blocked` 各类型的含义；新增"常用写法"：`eval --file`/`--max`、`open` 之后用 `wait` 而不是固定 sleep、`screenshot` 路径是位置参数、看大图用 `open --new-tab` 再 `tab close`、多个会话共用一个 Chrome 时哪些命令会切前台（`screenshot`、`find`、`vclick`、`turnstile`、`front`、`tab new`、`tab 序号`、`open --new-tab`）。去掉"不伪装轨迹"的说法，`move`/`clickat` 一直走的是拟人轨迹。
  - DESIGN.md："不做绕过人机验证的功能、不模拟人的鼠标轨迹"和已有的 `turnstile`、拟人轨迹前后矛盾，改为"不内置识别验证码内容、自动解题的逻辑，只提供要显式调用的通用输入操作"；补上长按、拖动的设计和测试说明。技术选型的依赖列表补上 `png`、`rusqlite`，"不新增依赖"改为"不为单个功能新增依赖"。
  - README.md：命令列表加上 `clickat --hold`、`drag`；支持 `text=文字` 的命令里补上 `text`；安装步骤加一步：把 SKILL.md 同步到 `~/.claude/skills/webctl/`（或做成指向仓库的目录链接）。之前本机技能目录里一直是 0.2.0 的旧说明。

## 0.3.0 - 2026-09-18

- 按文字定位：`click`、`hover`、`fill` 等命令的目标可以写 `text=文字`，找整段文字完全一致的可见元素，同源 iframe 里的也找。同一段文字有多处时，先排除被别的元素盖住的，再优先可点击的；还分不清就报错并列出候选，不替 agent 猜。
- `fill`：目标是标签文字时填进标签关联的输入框；目标拿不到输入焦点时报错。之前会把文字打进之前有焦点的那个输入框，而且不报错。
- `open`、`back`、`reload`：DOM 就绪后最多再等 3 秒 load 事件，没等到就返回 `"loaded": false` 并提示用 `wait`。广告多的站实测从 14–38 秒降到 7–8 秒，返回时正文已有最终字数的 94%–99%。
- `find`/`vclick`：修复小按钮一类的模板经常找不到的问题。目标坐标没对齐降采样网格时，粗匹配分数会掉到阈值以下被提前扔掉；实测一个文字按钮在 9 种对齐偏移里有 5 种找不到。粗匹配阈值改为比 `--threshold` 低 0.4，候选至少留 32 个。
- SKILL.md：要求 agent 每项任务设置 `WEBCTL_RUN_ID`，按文字找元素时用 `text=`，不要在 `eval` 里打标记再点。
- `eval`：按控制台（REPL）模式执行。同一页面上再次执行含 `const s = ...` 的脚本不再报 `Identifier 's' has already been declared`；`eval "await p"` 返回 `p` 的结果，之前返回 `null`。
- `--cdp` 接受 `http://127.0.0.1:端口` 写法，之前只认端口号和 `ws://`。
- 修复 Rust 1.97 下 `cargo clippy -- -D warnings` 报 `neg_cmp_op_on_partial_ord` 的问题。

## 0.2.0 - 2026-09-17

- 视觉识别：`find`（在截图里找模板图片）、`vclick`（找到后以拟人轨迹点击）、`move`、`clickat`（按视口坐标移动/点击，可点 canvas、封闭 shadow root）。
- `turnstile`：量出人机验证控件位置，用真实鼠标事件点勾选框；控件不可见时如实报出，不重试。
- 标签页：`tabs` 的序号按标签页首次出现的先后排，切换标签页后序号不再变化；`tab close` 后不会再把已关闭的页选为当前页。
- 帮助：`close` 说明为关闭整个浏览器，`tab`/`tabs` 补充用法说明。
- 截图：路径以 `.jpg`/`.jpeg` 结尾时保存为 JPEG（质量 80），体积约为 PNG 的四分之一。
- 日志：`webctl.db` 的 `commands` 表新增 `run_id`（取环境变量 `WEBCTL_RUN_ID`）和 `url`（命令返回的页面地址）两列，旧库自动补列。

## 0.1.0

首个版本：打开页面、快照编号、点击、填写、上传、截图、等待、对话框处理、命令日志等基础命令。
