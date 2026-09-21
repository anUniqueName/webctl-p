---
name: webctl
description: 用命令行操作 Chrome 浏览器。需要打开网页、读取页面内容、点击、填写表单、上传文件、截图，或在已登录的网站后台完成操作时使用。
---

# webctl

每条命令独立执行，浏览器在命令之间保持打开；登录状态保存在 webctl 自己的 Chrome 配置目录里，登录一次后一直有效。

## 基本流程

1. `webctl open <网址>`：打开页面（第一次会启动 Chrome）。
2. `webctl snapshot`：列出页面上可见的可操作元素，每个带编号，例如：

   ```
   url: https://example.com/login
   title: 登录
   scroll: 0/0  viewport: 1280x720
   [e1] textbox "用户名" placeholder="请输入用户名"
   [e2] textbox "密码" value="0 个字符"
   [e3] button "登录"
   ```

3. 按编号操作：`webctl fill e1 "张三"`、`webctl click e3`。也可以用 CSS 选择器：`webctl click "#submit"`，或按页面上的文字：`webctl click "text=冗余清单"`。`text=` 要求整段文字完全一致；同一段文字出现在多处、分不清是哪个时会报错并列出候选，这时改用编号。
4. 看返回的 `changes`：`navigated`（是否跳转）、`added`（新出现的文字，如"保存成功"）、`new_tabs`（新开的标签页）。有 `dialogs` 表示弹出过对话框。
5. 页面跳转或大面积刷新后编号会失效，重新 `snapshot`。

## 命令

| 命令 | 作用 |
|---|---|
| `open URL [--new-tab]` | 打开网址，本地文件路径也可以。返回 `"loaded": false` 表示图片、广告等还在加载，正文一般已经在了；要找的内容没出来就用 `wait` 等 |
| `snapshot [--in 目标] [--max N]` | 列出可操作元素并编号；`--in` 只看某个区域 |
| `text [目标] [--max N]` | 读页面或某个元素的文字 |
| `click 目标 [--double] [--right] [--force]` | 点击；被遮挡时报错并指出遮挡元素，`--force` 跳过检查 |
| `hover 目标` | 悬停，用于展开悬停菜单 |
| `fill 目标 文字 [--append]` | 清空后输入；`--append` 追加 |
| `type 文字` | 向当前焦点逐字按键输入，需要触发按键事件的输入框用这个 |
| `press 按键` | 如 `Enter`、`Tab`、`Escape`、`Control+A` |
| `select 目标 值或选项文字` | 原生下拉框；自定义下拉框先 `click` 展开，再 `snapshot` 找选项点击 |
| `upload 目标 文件...` | 给文件输入框设置文件 |
| `scroll up\|down\|top\|bottom [像素] [--in 目标]` | 滚动页面或某个区域 |
| `eval JS` / `eval --file 路径` `[--max N]` | 执行 JS 并返回结果；可以写 `return`、`await`，同一页面上多次 eval 可以重复声明同名 `const`，和页面自己的全局变量重名也不报错，不用包成 `(() => {...})()`；结果超过 `--max`（默认 20000 字）截成字符串，输出带 `truncated`、`length`，这时不能当 JSON 解析，加大 `--max` 重跑 |
| `screenshot [路径] [--full] [--annotate]` | 截图（会先把当前标签页切到前台）；路径是位置参数，直接写在后面，没有 `--path`；`--annotate` 在图上标出元素编号；路径写 `.jpg` 存成 JPEG，体积小，适合当证据。返回里有截图时的 `url`、`title`、`taken_at`（本地时间），可以核对拍到的是不是要的页面；文件已存在时照样覆盖，返回 `"overwrote": true` |
| `wait [--selector CSS] [--gone CSS] [--text 文字] [--url 片段] [--ms N] [--timeout 毫秒]` | 等待条件全部满足；`--selector` 等元素出现且可见，`script`、`style` 这类不渲染的元素按存在算（可以等 `#__NEXT_DATA__`） |
| `find 模板图 [--threshold 0.8] [--max N]` | 视觉识别：在页面截图里找模板图片（PNG/JPEG/WebP），返回匹配中心坐标和相似度 |
| `vclick 模板图 [--threshold 0.8]` | 找到模板图位置并以拟人轨迹移动点击；找不到就报错，不盲点 |
| `gap 背景图 [--piece 滑块图] [--max N]` | 滑块验证码：在背景图（PNG/JPEG/WebP）里找缺口位置，纯本地计算不需要浏览器 |
| `move X Y` / `clickat X Y [--right] [--double] [--hold 毫秒]` | 拟人轨迹移动 / 点击视口坐标，不经过 DOM 定位，可点 canvas、封闭 shadow root；`--hold` 是长按，按下保持指定毫秒再松开 |
| `drag X1 Y1 X2 Y2 [--duration 毫秒]` | 按住左键从起点拖到终点（拟人轨迹，默认用 800ms），用于滑块、拖拽排序 |
| `tabs` / `tab 序号` / `tab new [URL]` / `tab close [序号]` | 标签页管理，序号从 0 开始；关掉当前页后新的当前页不会选别的会话正在用的页；剩下的页都是别的会话的时返回 `"current": null`，不新建页，下一条命令会自动选页或新建空白页 |
| `back` / `reload` | 后退 / 刷新 |
| `turnstile [--timeout 毫秒]` | 先切到前台，量出验证控件位置，用真实鼠标事件点一下勾选框；控件不可见时如实报出，不重试 |
| `front` | 把浏览器窗口切到前台 |
| `status` / `close` | 查看会话 / 关闭浏览器 |

全局参数：`--session 名字`（多个互相独立的浏览器）、`--headless`（无界面，只在启动浏览器时生效）、`--cdp 端口`（连接已开调试端口的浏览器）、`--on-dialog accept|dismiss`、`--prompt-text 文字`。

每条命令都会记进日志。开始一项任务时定一个任务标识（如 `price-audit-20260918T1500`），这次任务的每条命令都带上环境变量 `WEBCTL_RUN_ID`，事后能按它把这次任务的命令归到一起排查。agent 每次调用 shell 通常不保留环境变量，所以每条命令前都写上：bash 里是 `WEBCTL_RUN_ID=price-audit-20260918T1500 webctl click e3`，PowerShell 里是 `$env:WEBCTL_RUN_ID='price-audit-20260918T1500'; webctl click e3`。

## 常用写法

- 打开后等内容出来：`webctl open URL && webctl wait --text "加入购物车" --timeout 15000`，不要 `sleep` 固定秒数。多条命令用 `&&` 连，前一条失败就停下。
- 多行 JS、含 `$` 或 `\` 的 JS（正则常有）先写进文件，再 `webctl eval --file 脚本.js`。用 `eval "$(cat 脚本.js)"` 传时 shell 会改掉里面的字符，正则被改坏后返回空结果，容易被当成"没找到"。
- 截图：`webctl screenshot shots/item-173015.jpg`，文件名带上时分秒，免得和别的截图重名被覆盖。
- 只读某个区域的文字：`webctl text main`、`webctl text "#price"`。
- 看大图：`webctl open --new-tab 图片地址`，看完 `webctl tab close`。还开着别的标签页时，用 `tabs` 确认当前页，必要时 `tab 序号` 切回。
- 多个会话（`--session`，第二个起用 `--cdp`）共用一个 Chrome 时，窗口是共用的，同一时刻只有一个标签页在前台。会把本会话标签页切到前台的命令：`screenshot`、`find`、`vclick`、`turnstile`、`front`、`tab new`、`tab 序号`、`open --new-tab`。后台标签页不出画面，两个会话同时跑这些命令会互相抢前台、截图等不到结果，要一个接一个执行；`click`、`fill`、`snapshot`、`text`、`eval` 在后台标签页上照常能用。

## 对话框

alert、confirm、prompt 在弹出它的那条命令里当场处理，默认按"取消"。输出的 `dialogs` 写明对话框内容和处理方式。确认要执行（比如确实要删除）时，加 `--on-dialog accept` 重新执行该操作：

```
webctl click e7                      # 输出 dialogs: confirm "确定删除吗？" 已取消
webctl click e7 --on-dialog accept   # 确认删除
```

## 点了没反应怎么判断

`click` 发的是浏览器输入层的真实鼠标事件（页面上监听到的 `mousemove → mousedown → mouseup → click` 四个事件 `isTrusted` 都是 `true`），不是 JS 合成的 `el.click()`。所以**返回里没有 `changes` 不等于点击没生效**，多半是表单提交或跳转比默认等待时间长，webctl 先返回了。

按这个顺序排查，不要一上来就改用 `eval` 里 `el.click()` 绕过去：

1. `webctl click e3 --settle 6000` 把等待加长重试；
2. 或者点完立刻用 `webctl wait --url 目标地址片段 --timeout 10000` 等确切条件；
3. 还是没动静，再用 `webctl text` 或 `screenshot` 看页面到底成了什么样。

## 登录和验证码

需要登录、扫码时：运行 `webctl front` 把窗口切到前台，请用户手动完成，然后用 `webctl wait --url 登录后地址的片段 --timeout 300000`（或 `--selector`、`--text`）等待，再继续。

遇到人机验证，默认也这样请用户处理。调用 webctl 的技能明确允许 agent 自己处理验证时，按那个技能的规定做（能试几次、试不过怎么记）。webctl 提供的只是通用输入和图像识别原语：`turnstile`（量出 Cloudflare 控件位置，点一下勾选框）、`clickat`、`clickat --hold`（长按）、`drag`（按住拖动）、`find`/`vclick`（按截图里的图案找位置再点）、`gap`（在滑块背景图里找缺口坐标）。它不自动解题：点哪里、按多久、拖到哪个候选，都由调用方看截图后给出。

`open`、`back`、`reload`、`eval` 认出拦截页或错误页时，返回里会带 `"blocked": "<类型>"` 和一条 `hint`。这时页面上没有目标内容，别去 snapshot 或 text 找数据，也别当成"0 个结果"。点筛选、翻页后跳出来的拦截页不经过 `open`，所以 `eval` 的返回也要看有没有 `blocked`。

| `blocked` | 页面 | 一般怎么处理（最终按调用方自己的规定） |
|---|---|---|
| `cloudflare` | Cloudflare 人机验证（Just a moment… 页，或正文很短、只有勾选框的验证页；正常页面的评论、结账表单里嵌的勾选框不报） | 先试 `webctl turnstile`（见下）；不行再交人工或记为拦截 |
| `perimeterx` | 长按验证（如 Walmart 的 Robot or human?） | 需要按住才能过：交人工，或按调用方的规定用 `clickat X Y --hold 毫秒` |
| `puzzle` | 拼图、滑块验证（如 TikTok 的 Drag the puzzle piece） | 需要拖动才能过：交人工，或按调用方的规定用 `drag X1 Y1 X2 Y2` |
| `akamai` | 站点拒绝访问（Akamai 的 Access Denied 页） | 记为拦截 |
| `datadome` | DataDome 验证页。有的是设备检查页，几秒后自己跳回原页（地址带 `dd_referrer`，Etsy 首页实测如此） | 先 `wait --gone 'iframe[src*="captcha-delivery.com"]' --timeout 10000` 再读；还在就是拒绝访问，记为拦截 |
| `ebay_interstitial` | eBay 验证过渡页（地址含 `/splashui/`） | 有时几秒后自己跳回，用 `wait --url 目标地址片段` 等一下；不跳就交人工或记为拦截 |
| `other` | 标题像拦截页或错误页（Pardon Our Interruption、Security Check、Error Page 等），正文很短 | 先 `screenshot` 看一眼再决定 |

交人工时：

```sh
webctl front                                                  # 切到前台
# 请用户手动完成验证
webctl wait --text "目标页上的文字" --timeout 300000            # 等目标内容出来
```

别用 `wait --gone "#challenge-form"` 等：有的验证页只靠标题认出来，页面上本来就没有这个元素，会立刻返回。

验证页上有可见控件时，可以先试 `webctl turnstile`：先把标签页切到前台，量出控件在页面上的位置（最少量 3 秒，`--timeout` 更长就多量，最多 10 秒），用和真人一样的真实鼠标事件点一下勾选框的位置，再等验证消失。这是显式命令，`open` 不会自动去点。

```sh
webctl turnstile   # {"challenge":"widget_visible","measured_from":"container","clicked":true,"passed":true}
```

`measured_from` 说明位置是从哪量的：`iframe` 是控件就是页面里的 iframe；`container` 是控件内部在封闭 shadow root 里够不着，退回用容器的位置——真实 Turnstile 基本都是后者。整页验证的主文档里常常只有一个 0×0 的隐藏字段 `cf-chl-widget-xxx_response`，这时量的是它的父元素（挂封闭 shadow root 的容器）。

判断只到"控件可不可见"为止，到不了"能不能点"：控件内部谁也进不去。所以：

- `"passed":true` —— 点完验证消失了，继续干活。
- `"passed":false` —— 点过了但验证还在，说明这个控件不是点一下能过的，**别重试**，按调用方的规定交人工或记为拦截。
- `"challenge":"widget_hidden"` —— 量了 3 秒以上都没量到可见控件。先 `screenshot` 看一眼：看得到勾选框时，按调用方的规定决定是否用 `clickat` 点它；看不到就是非交互式验证，放不放行取决于浏览器环境和出口 IP，交人工或记为拦截。

过一次之后验证 cookie 存在该会话的配置目录里，之后一段时间（通常几小时到几天）同一个 session 访问不会再拦，所以这一步不是每次跑都要。

被网站挡住时先想 cookie，别归咎于"自动化被识别"。webctl 用的是自己的 Chrome 配置目录，新建的目录一条 cookie 都没有，亚马逊这类站点对全新访客本来就会拦。让用户手动登录一次，配置目录攒上 cookie，后面就正常了。webctl 不改浏览器指纹，也不需要——真实输入事件本身没有自动化标记。

## 注意

- 要按文字找元素时用 `text=文字`，不要在 `eval` 里按文字找到元素、打上属性再用选择器去点。
- 新标签页不会自动切换：看到 `new_tabs` 后用 `tabs` 查看、`tab 序号` 切过去。
- 元素在视口外时 `snapshot` 会标 `(视口外)`，点击时自动滚动过去。
- 图标按钮、canvas 这类看不出含义的元素，用 `screenshot --annotate` 截图对照编号。
- 跨域 iframe 里的元素无法编号和操作。
- 命令报"页面没有响应"时，多半是页面在命令间隙自己弹了对话框：`webctl front` 后请用户手动关闭。

## 视觉识别定位

DOM 选择器够不着的东西（canvas、图标按钮、封闭 shadow root、跨域 iframe 里的画面）用模板图片定位：

```sh
webctl screenshot page.png          # 截当前视口
# 从 page.png 裁出按钮/图标那一小块存成 button.png
webctl find button.png              # {"matches":[{"x":320.5,"y":120,"w":24,"h":24,"score":0.97}]}
webctl vclick button.png            # 或者直接找+移+点一步完成
```

模板必须和页面同一缩放比例、同一 devicePixelRatio，从刚截的图里裁最稳；匹配对亮度对比度变化不敏感，但页面缩放（Ctrl+滚轮）变了就要重裁。`find` 返回多个匹配加 `--max N`；匹配不上时适当降低 `--threshold`（默认 0.8）。`clickat` 按坐标直接点，没有遮挡检查。

## 滑块验证码

`gap` 在滑块背景图里找缺口位置，配合 `drag` 完成拖动，全程本地计算、不需要训练模型：

```sh
# 拿到背景图和滑块小块：eval 里读 img.src / canvas.toDataURL 存成文件，或对元素截图
webctl gap bg.png --piece piece.png
# {"candidates":[{"x":153,"y":90,"w":42,"h":45,"cx":174,"cy":112,"score":9.96,"iou":0.74,...}], ...}
webctl drag <滑块把手x> <滑块把手y> <把手x + 缺口x*显示比例> <把手y>
```

原理：缺口是一块被压暗、边缘带亮边的区域。`gap` 算每个像素相对大窗口局部均值的变暗量，在多个阈值下取连通域，跨阈值稳定、暗得明显、尺寸和滑块相当的候选排前面；给了 `--piece` 还会算连通域和滑块形状（alpha 通道）的 IoU。多缺口干扰时几个候选都会返回，第一个拖不过就试下一个（验证码本身允许重试）。

注意：背景图要用**页面上实际显示的那张**（`eval` 取 `img.currentSrc` 下载，或对元素截图），别用打乱的原始切片图；图和滑块要同一缩放比例。坐标是图片像素，乘 `显示宽度/图片宽度` 换算成拖动距离。`gap` 只给坐标，拖不拖、拖到哪个候选、过没过由你判断。
