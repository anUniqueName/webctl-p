# webctl

`webctl` 是给 Claude Code、Codex 等 agent 使用的 Chrome 命令行控制工具。每条命令独立连接浏览器，快照中的元素编号会写入页面 DOM，操作后直接返回页面变化。

## 安装

不装 Rust：到 [Releases](https://github.com/anUniqueName/webctl-p/releases) 下载对应版本的 `webctl-windows-x86_64.zip`，把里面的 `webctl.exe` 放进 PATH 里的目录（如 `%USERPROFILE%\.cargo\bin`）。

按标签从 GitHub 装：`cargo install --git https://github.com/anUniqueName/webctl-p.git --tag v0.6.0`。

从源码装：

```sh
cargo install --path .
```

再把仓库里的 `SKILL.md` 同步到 `~/.claude/skills/webctl/`，否则 agent 读到的还是旧版说明。每次升级后复制一次，或者把技能目录做成指向仓库的目录链接，之后不用再管（Windows：`mklink /J "%USERPROFILE%\.claude\skills\webctl" <仓库目录>`，建之前先删掉旧的 `webctl` 目录）。

可用 `WEBCTL_HOME` 指定数据目录，`WEBCTL_SESSION` 或 `--session` 选择会话，`WEBCTL_CHROME` 指定 Chrome/Edge 可执行文件。也可用 `--cdp <端口|http地址|ws地址>` 连接已有浏览器。

## 命令

`TARGET` 可以是 snapshot 编号（`e3`）或 CSS 选择器。`click`、`hover`、`fill`、`select`、`upload`、`scroll --in`、`text` 还可以写 `text=文字`，按页面上整段文字完全一致来找；同一段文字有多处、分不清是哪个时报错。

- `open [URL] [--new-tab] [--timeout MS]`：打开地址或启动浏览器。DOM 就绪后最多再等 3 秒 load 事件，没等到就先返回 `"loaded": false`，免得广告多的站要等十几秒；`--timeout` 是总的等待上限（默认 30000）。
- `snapshot [--in TARGET] [--max N]`：列出可见可操作元素并编号。
- `text [TARGET] [--max N]`：读取页面或元素文字。
- `click TARGET [--force] [--right] [--double]`、`hover TARGET`：点击或悬停元素。目标被别的元素挡住时 `click` 报错并指出遮挡的元素，`--force` 跳过这个检查照原位置点。
- `fill TARGET TEXT [--append]`、`type TEXT [--delay MS]`、`press KEY`、`select TARGET VALUE`：输入与键盘操作。`fill` 默认先清空再输入，`--append` 追加；`type` 逐字发按键事件，`--delay` 是字间隔毫秒数（默认 0）。
- `upload TARGET FILE...`：设置文件输入框。
- `scroll up|down|top|bottom [PX] [--in TARGET]`：滚动页面；`--in` 滚动某个元素内部。
- `eval JS` / `eval --file PATH` `[--max N]`：执行 JavaScript。按控制台模式执行：同一页面上多次 eval 可以重复声明同名 `const`/`let`，顶层 `await` 直接返回结果。多行脚本、含 `$` 或 `\` 的脚本写进文件用 `--file`；结果超过 `--max`（默认 20000 字）截成字符串，输出带 `truncated`、`length`。
- `screenshot [PATH] [--full] [--annotate]`：保存截图。路径以 `.jpg`/`.jpeg` 结尾时存成 JPEG（质量 80），否则 PNG；`find`/`vclick`/`gap` 的图片支持 PNG/JPEG/WebP。输出含截图时页面的 `url`、`title`、`taken_at`（本地时间）；文件已存在时照样覆盖，输出 `"overwrote": true`。
- `find IMAGE [--threshold 0.8] [--max N]`：视觉识别——在当前页面截图里找模板图片（PNG/JPEG/WebP），返回匹配中心的视口坐标与相似度。
- `move X Y`：以拟人轨迹（贝塞尔曲线、先慢后快再慢）把鼠标移到视口坐标，只移动不点击。
- `clickat X Y [--right] [--double] [--hold MS]`：移动并点击视口坐标，不经过 DOM 定位；`--hold` 是长按，按下保持 MS 毫秒再松开。
- `drag X1 Y1 X2 Y2 [--duration MS]`：按住左键从 (X1, Y1) 拖到 (X2, Y2)，移动时带着按住的左键，默认用 800 毫秒拖完。用于滑块、拖拽排序等。
- `vclick IMAGE [--threshold 0.8] [--right] [--double]`：`find` + `clickat` 一步完成；找不到匹配就报错，不盲点。
- `gap BG [--piece PIECE] [--max N]`：滑块验证码缺口识别——在背景图（PNG/JPEG/WebP）里找被压暗的缺口区域，返回按分数排序的候选坐标；纯本地计算，不需要浏览器。给了 `--piece` 滑块图会按形状吻合度打分，多缺口干扰时返回多个候选，配合 `drag` 完成拖动。
- `wait`：等待选择器、文字、URL、元素消失或固定时间。
- `turnstile [--timeout MS]`：点一下交互式人机验证（Cloudflare Turnstile）控件上的勾选框——先切到前台，量出控件位置，发一次真实鼠标点击，再等验证消失。只点一次不重试；量不到可见控件时如实报出，不做任何绕过。
- `front`：将当前页面切到前台。
- `tabs`、`tab ...`：列出、切换、新建或关闭标签页。
- `back`、`reload`：后退或刷新。
- 会改变页面的命令（`click`、`clickat`、`vclick`、`hover`、`fill`、`type`、`press`、`select`、`drag`）都有 `--settle` 和 `--timeout`：操作后先等页面稳定下来再报变化，`--settle`（默认 3000）是等待上限，期间页面连续 300 毫秒没变化就提前结束；如果这期间页面发生了跳转，改为等加载完成，上限是 `--timeout`（默认 30000）。返回里没有变化而操作其实生效了，多半是提交或跳转比 `--settle` 慢，加大它重试。
- 全局参数 `--on-dialog accept|dismiss`（默认 dismiss）：页面弹出对话框时点确定还是取消；`--prompt-text` 给 prompt 输入内容。对话框在弹出它的命令里当场处理，结果记录在输出的 `dialogs` 里。
- `status`、`close`：查看或关闭会话。

## 视觉识别与鼠标模拟

`find`/`vclick` 的模板图片从 `webctl screenshot` 截的图里裁出来即可。匹配用的是灰度归一化互相关，对亮度和对比度变化不敏感，但**不做缩放不变性**：页面缩放（Ctrl+滚轮）或设备像素比变了，模板就要重裁。坐标一律是视口 CSS 像素，和 `clickat`、`move` 通用。

`move`/`clickat`/`vclick`/`drag` 的移动轨迹模拟真人：从上一次停下的位置出发（按会话记在 `<WEBCTL_HOME>/sessions/<会话>.mouse`），走带随机弯曲的贝塞尔曲线，先慢后快再慢，落点精确。点击仍是浏览器级真实鼠标事件，只是目标从 DOM 元素换成了坐标，可以点 canvas、封闭 shadow root 等选择器够不着的地方。这些都是通用输入操作：webctl 不自动解题，点哪里、按多久、拖到哪由调用方给出；`gap` 只回答"缺口在图里哪个位置"这个纯图像问题，拖不拖、拖到哪个候选、过没过都仍由调用方判断。

## 日志

每条命令执行完会往 `<WEBCTL_HOME>/webctl.db`（SQLite）追加一行，表 `commands`：

| 列 | 含义 |
|---|---|
| `ts` | 本地时间 |
| `session` | 会话名 |
| `command` | 子命令，如 `click` |
| `argv` | 完整命令行参数，JSON 数组 |
| `ok` | 是否成功 |
| `ms` | 耗时毫秒 |
| `error` | 失败时的错误信息 |
| `navigated` / `added_count` / `new_tabs` | 该命令返回的 `changes` 里的三个计数，用来排查慢命令；页面正文不记 |
| `run_id` | 环境变量 `WEBCTL_RUN_ID` 的值，调用方用它把一批命令归到同一次任务；没设为空。SKILL.md 里要求 agent 每项任务都设置 |
| `url` | 命令返回里的页面地址（`open`、`back`、`reload` 和有页面变化的操作才有），不为此多查浏览器 |
| `version` | 产生这一行的 webctl 版本，跨版本收集日志时用来对上是哪个构建 |
| `hint` | 命令输出里给 agent 的提示（`gap`、`turnstile` 用的键名是 `note`，也记在这里），事后能看出它是被提醒过还是根本没收到提示 |
| `loaded` | `open`、`back`、`reload` 输出的 `loaded`，是否等到了页面完全加载 |

命令行写错（clap 解析不过）也会记一行：`command` 从命令行里认出来，`ok` 为 0，`error` 是 clap 报错的第一行；stderr 的报错文字和退出码 2 都和以前一样。`--help`、`--version` 不记。

写日志出错会被忽略，不影响命令本身。设 `WEBCTL_LOG=0` 关闭记录。

**密码不进日志**：`fill` 的目标、`type` 时当前有焦点的元素是 `input[type=password]` 时，`argv` 里那段文字记成 `***`（命令输出里也会多一个 `"masked": true`）。`fill`、`type` 失败时也一律记成 `***`：这时还判断不出目标是不是密码框，而排查失败看的是目标和错误原因。仍有一种情况没覆盖，注意避开：`eval` 的脚本一律原样记录，别把密码写进 `eval` 的脚本里。

查最近失败的命令：

```sh
sqlite3 "$WEBCTL_HOME/webctl.db" "select ts, command, argv, error from commands where ok = 0 order by id desc limit 20"
```

查某次任务里最慢的命令：

```sh
sqlite3 "$WEBCTL_HOME/webctl.db" "select ts, command, ms, url, error from commands where run_id = '<run_id>' order by ms desc limit 10"
```

### 收集日志

数据库开着 WAL 模式，最近写入的行可能还在 `webctl.db-wal` 里没并回主文件。只拷 `webctl.db` 一个文件会漏掉这些行，两种做法任选：

- 确认没有 webctl 命令在跑，然后把 `webctl.db`、`webctl.db-wal`、`webctl.db-shm` 三个文件一起拷走；
- 或者在跑的时候取一份一致的副本：

  ```sh
  sqlite3 "$WEBCTL_HOME/webctl.db" "VACUUM INTO 'copy.db'"
  ```

  没有 `sqlite3` 命令时用 Python 的 `sqlite3` 备份接口：`sqlite3.connect(src).backup(sqlite3.connect(dst))`。

日志只追加，不会自动删除，也没有轮转：长期跑的机器要自己定期归档或清理。

## 安全说明

Chrome 调试端口仅绑定本机，但本机其他进程仍可连接并控制浏览器。会话配置目录会持久保存登录 Cookie，请保护 `WEBCTL_HOME`。`eval` 能在当前页面执行任意 JavaScript，只执行可信脚本。

日志里的 `argv` 是原样记录的命令行。填进密码框的内容、以及 `fill`、`type` 失败时填的文字都会换成 `***`（见上文"日志"），但 `eval` 的脚本仍是原样。这个库和 Cookie 在同一个目录，保护级别一致；不想记就设 `WEBCTL_LOG=0`。

## 发布

1. 改 `Cargo.toml` 的 `version`，在 CHANGELOG.md 最上面写 `## X.Y.Z - 日期` 一节，这一节会原样当作发布说明。
2. 本机跑 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（CI 不跑 fmt 和 clippy：它的 Rust 版本更新后，新加的 lint 会卡住发版）。
3. 提交，打标签并推送：`git tag -a vX.Y.Z -m "webctl X.Y.Z"`，`git push origin main vX.Y.Z`。
4. GitHub Actions（`.github/workflows/release.yml`）在 Windows 上核对标签和 `Cargo.toml` 版本一致、跑 `cargo test`、编译，把 `webctl.exe` 连同 SKILL.md、README.md、CHANGELOG.md、LICENSE 打成 `webctl-windows-x86_64.zip` 发到 Releases。测试不过就不发。

## 许可

MIT，见 [LICENSE](LICENSE)。
