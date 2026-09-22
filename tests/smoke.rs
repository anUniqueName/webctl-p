use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn run(home: &Path, args: &[&str]) -> Output {
    run_as(home, "smoke", args)
}

fn run_as(home: &Path, session: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_webctl"))
        .args(["--session", session, "--headless"])
        .args(args)
        .env("WEBCTL_HOME", home)
        .output()
        .expect("无法执行 webctl")
}

fn json_as(home: &Path, session: &str, args: &[&str]) -> Value {
    let output = run_as(home, session, args);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "命令失败 {session} {args:?}: {stdout}"
    );
    serde_json::from_str(stdout.trim()).unwrap()
}

fn ok(home: &Path, args: &[&str]) -> String {
    let output = run(home, args);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "命令失败 {args:?}: {stdout}");
    stdout
}

fn json(home: &Path, args: &[&str]) -> Value {
    serde_json::from_str(ok(home, args).trim()).unwrap()
}

fn ref_for(snapshot: &str, label: &str) -> String {
    snapshot
        .lines()
        .find(|line| line.contains(label))
        .and_then(|line| line.strip_prefix('['))
        .and_then(|line| line.split(']').next())
        .unwrap_or_else(|| panic!("快照中找不到 {label}:\n{snapshot}"))
        .to_owned()
}

/// status 和连不上的 --cdp 都不会启动 Chrome，所以这个测试不依赖浏览器
#[test]
fn logs_commands_to_sqlite() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = root.join("target/log-home");
    let _ = fs::remove_dir_all(&home);

    json(&home, &["status"]);
    let failed = Command::new(env!("CARGO_BIN_EXE_webctl"))
        .args([
            "--session",
            "smoke",
            "--headless",
            "click",
            "e1",
            "--cdp",
            "1",
        ])
        .env("WEBCTL_HOME", &home)
        .env("WEBCTL_RUN_ID", "job-42")
        .output()
        .expect("无法执行 webctl");
    assert!(!failed.status.success(), "连不上的端口应该报错");

    let db = rusqlite::Connection::open(home.join("webctl.db")).unwrap();
    let rows: Vec<(String, bool, Option<String>, String)> = db
        .prepare("SELECT command, ok, error, argv FROM commands ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert_eq!(rows.len(), 2, "应记下两条命令：{rows:?}");
    assert_eq!(rows[0].0, "status");
    assert!(rows[0].1, "status 应记为成功");
    assert_eq!(rows[1].0, "click");
    assert!(!rows[1].1, "连不上时 click 应记为失败");
    assert!(rows[1].2.is_some(), "失败的命令应记下错误");
    assert!(rows[1].3.contains("e1"), "argv 应完整记下：{}", rows[1].3);
    let run_ids: Vec<Option<String>> = db
        .prepare("SELECT run_id FROM commands ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        run_ids,
        [None, Some("job-42".to_owned())],
        "WEBCTL_RUN_ID 应记进 run_id"
    );

    // WEBCTL_LOG=0 关闭记录
    Command::new(env!("CARGO_BIN_EXE_webctl"))
        .args(["--session", "smoke", "status"])
        .env("WEBCTL_HOME", &home)
        .env("WEBCTL_LOG", "0")
        .output()
        .expect("无法执行 webctl");
    let count: i64 = db
        .query_row("SELECT count(*) FROM commands", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2, "WEBCTL_LOG=0 时不应写入");

    // 每行都记下产生它的 webctl 版本：收集来的日志跨多个版本，没有它对不上是哪个构建
    let versions: Vec<Option<String>> = db
        .prepare("SELECT version FROM commands ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        versions
            .iter()
            .all(|version| version.as_deref() == Some(env!("CARGO_PKG_VERSION"))),
        "version 列应填当前版本：{versions:?}"
    );

    // 命令行写错也要留下记录：clap 解析不过时进程直接结束，之前一行都不记
    let bad = Command::new(env!("CARGO_BIN_EXE_webctl"))
        .args(["--session", "smoke", "status", "--nope"])
        .env("WEBCTL_HOME", &home)
        .output()
        .expect("无法执行 webctl");
    assert_eq!(bad.status.code(), Some(2), "退出码照旧是 2");
    let bad_stderr = String::from_utf8_lossy(&bad.stderr).into_owned();
    assert!(
        bad_stderr.contains("--nope"),
        "clap 的报错照旧写 stderr：{bad_stderr}"
    );
    let (bad_command, bad_ok, bad_error): (String, bool, Option<String>) = db
        .query_row(
            "SELECT command, ok, error FROM commands ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(bad_command, "status", "子命令名要从 argv 里认出来");
    assert!(!bad_ok, "命令行写错应记为失败");
    assert!(
        bad_error
            .as_deref()
            .is_some_and(|e| e.contains("--nope") && !e.contains('\u{1b}')),
        "应记下 clap 的报错，且不带终端配色的转义字符：{bad_error:?}"
    );

    // --help 是正常输出，不记
    Command::new(env!("CARGO_BIN_EXE_webctl"))
        .arg("--help")
        .env("WEBCTL_HOME", &home)
        .output()
        .expect("无法执行 webctl");
    let count: i64 = db
        .query_row("SELECT count(*) FROM commands", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3, "--help 不该记一行失败");

    // fill 失败时还判断不出目标是不是密码框，文字一律记成 ***（这里连不上端口，失败得更早）
    let failed_fill = Command::new(env!("CARGO_BIN_EXE_webctl"))
        .args([
            "--session",
            "smoke",
            "--headless",
            "fill",
            "#nope",
            "some-secret-text",
            "--cdp",
            "1",
        ])
        .env("WEBCTL_HOME", &home)
        .output()
        .expect("无法执行 webctl");
    assert!(!failed_fill.status.success(), "连不上的端口应该报错");
    let fill_argv: String = db
        .query_row(
            "SELECT argv FROM commands WHERE command = 'fill' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        fill_argv.contains("***") && fill_argv.contains("#nope"),
        "失败的 fill 应遮住文字、留下目标：{fill_argv}"
    );
    let leaked: i64 = db
        .query_row(
            "SELECT count(*) FROM commands WHERE argv LIKE '%some-secret-text%' OR ifnull(error, '') LIKE '%some-secret-text%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "整张表里都不该出现填进去的文字");
}

#[test]
fn chrome_smoke() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = root.join("target/smoke-home");
    let _ = fs::remove_dir_all(&home);
    let fixture = root.join("tests/fixture.html");
    let fixture_text = fixture.to_string_lossy().into_owned();

    let opened = run(&home, &["open", &fixture_text]);
    let opened_stdout = String::from_utf8(opened.stdout).unwrap();
    if !opened.status.success()
        && (opened_stdout.contains("找不到 Chrome") || opened_stdout.contains("无法启动"))
    {
        eprintln!("跳过 smoke：{}", opened_stdout.trim());
        return;
    }
    assert!(
        opened.status.success(),
        "打开 fixture 失败：{opened_stdout}"
    );

    let snapshot = ok(&home, &["snapshot"]);
    for label in ["姓名", "保存", "打开新标签", "Germany", "iframe按钮"] {
        assert!(snapshot.contains(label), "快照缺少 {label}:\n{snapshot}");
    }
    // 页面把元素的 tagName 改成 undefined 也照样列出；读取出错的元素跳过并报个数，不让整个快照失败
    assert!(snapshot.contains("改过标签名"), "{snapshot}");
    assert!(snapshot.contains("1 个元素读取出错，已跳过"), "{snapshot}");

    let select_ref = ref_for(&snapshot, "Germany");
    let scoped = ok(&home, &["snapshot", "--in", &select_ref]);
    assert!(
        scoped.contains("Germany") && !scoped.contains("保存"),
        "--in 未限定范围：\n{scoped}"
    );

    assert_eq!(
        json(&home, &["eval", "return document.title"])["result"],
        "webctl smoke fixture"
    );
    assert_eq!(json(&home, &["eval", "({n: 1 + 1})"])["result"]["n"], 2);
    // 超过 --max：截成字符串，并给出完整长度
    let cut = json(&home, &["eval", "'x'.repeat(50)", "--max", "10"]);
    assert_eq!(cut["truncated"], true, "{cut}");
    assert_eq!(cut["length"], 52, "序列化后带两个引号：{cut}");
    // script 标签不渲染，wait --selector 按存在算（Walmart 的 #__NEXT_DATA__）
    assert_eq!(
        json(
            &home,
            &["wait", "--selector", "script", "--timeout", "2000"]
        )["ok"],
        true
    );
    // 同一页面上重复声明同名 const 不报错；顶层 await 和表达式返回的 Promise 都拿到结果
    assert_eq!(json(&home, &["eval", "const s = 1; s"])["result"], 1);
    assert_eq!(json(&home, &["eval", "const s = 2; s"])["result"], 2);
    assert_eq!(
        json(&home, &["eval", "await Promise.resolve(3)"])["result"],
        3
    );
    assert_eq!(json(&home, &["eval", "Promise.resolve(4)"])["result"], 4);
    // 和页面自己的全局变量（fixture 里的 var webctlPageVar）重名也不报"已声明"，顶层 await 照样能用
    assert_eq!(
        json(&home, &["eval", "const webctlPageVar = 2; webctlPageVar"])["result"],
        2
    );
    assert_eq!(
        json(
            &home,
            &[
                "eval",
                "const webctlPageVar = await Promise.resolve(5); webctlPageVar"
            ]
        )["result"],
        5
    );
    // 失败时 stdout 照旧输出 JSON，stderr 另有一行，接了 grep 管道也看得到
    let failed_eval = run(&home, &["eval", "noSuchFunction()"]);
    assert!(!failed_eval.status.success());
    assert!(
        String::from_utf8_lossy(&failed_eval.stdout).contains("\"ok\":false"),
        "stdout 格式不能变"
    );
    let failed_stderr = String::from_utf8_lossy(&failed_eval.stderr);
    assert!(
        failed_stderr.starts_with("webctl: ") && failed_stderr.contains("noSuchFunction"),
        "{failed_stderr}"
    );

    let filled = json(&home, &["fill", "#name", "中文输入"]);
    assert_eq!(filled["value"], "中文输入");

    let clicked = json(&home, &["click", "#flash"]);
    assert!(
        clicked["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "保存成功"),
        "未捕获瞬时提示：{clicked}"
    );

    let blocked = run(&home, &["click", "#blocked"]);
    let blocked_stdout = String::from_utf8(blocked.stdout).unwrap();
    assert!(
        !blocked.status.success(),
        "被遮挡按钮不应点击成功：{blocked_stdout}"
    );
    assert!(
        blocked_stdout.contains("遮挡"),
        "错误未说明遮挡：{blocked_stdout}"
    );
    assert!(
        blocked_stdout.contains("--force"),
        "遮挡报错应告诉 agent 下一步怎么办：{blocked_stdout}"
    );
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("遮挡"),
        "失败时 stderr 也要有错误"
    );

    // 正在淡出的遮罩层：点击该重试等它消失，而不是直接报遮挡
    ok(
        &home,
        &[
            "eval",
            "setTimeout(() => document.querySelector('#fading-mask').remove(), 400); return 'armed'",
        ],
    );
    let started = std::time::Instant::now();
    json(&home, &["click", "#fading"]);
    assert!(
        started.elapsed().as_millis() >= 400,
        "遮罩层还在时应该等一下再点：{:?}",
        started.elapsed()
    );

    // wait --selector 的可见判定要和 click 一致：元素还是 opacity:0 时不能放行，
    // 否则 wait 刚返回、click 就报"不可见"
    ok(
        &home,
        &[
            "eval",
            "setTimeout(() => document.querySelector('#fade-in').style.opacity = '1', 600); return 'armed'",
        ],
    );
    let started = std::time::Instant::now();
    assert_eq!(
        json(
            &home,
            &["wait", "--selector", "#fade-in", "--timeout", "5000"]
        )["ok"],
        true
    );
    assert!(
        started.elapsed().as_millis() >= 600,
        "元素还是 opacity:0 时 wait 不该放行：{:?}",
        started.elapsed()
    );
    json(&home, &["click", "#fade-in"]);
    assert_eq!(
        json(
            &home,
            &["eval", "document.querySelector('#text-result').textContent"]
        )["result"],
        "淡入按钮",
        "wait 返回后应当立刻点得到"
    );

    json(&home, &["press", "Ctrl+A"]);

    // 标题文字盖住 checkbox 但同在一个 label 里，点下去照样生效，不算遮挡
    json(&home, &["click", "#opt"]);
    assert_eq!(
        json(
            &home,
            &["eval", "return document.querySelector('#opt').checked"]
        )["result"],
        true,
        "label 内被标题盖住的 checkbox 应该点得到"
    );

    let bad_selector = run(&home, &["click", "button:has-text('保存')"]);
    let bad_stdout = String::from_utf8(bad_selector.stdout).unwrap();
    assert!(
        bad_stdout.contains("不是合法的 CSS 选择器"),
        "非法选择器应给出可读提示：{bad_stdout}"
    );

    // text=文字 按页面文字定位
    let text_result = |home: &Path| {
        json(
            home,
            &["eval", "document.querySelector('#text-result').textContent"],
        )["result"]
            .clone()
    };
    json(&home, &["click", "text=确定"]);
    assert_eq!(
        text_result(&home),
        "前面的确定",
        "被遮罩盖住的同名按钮不该选中"
    );
    json(&home, &["click", r#"text="新增物流渠道""#]);
    assert_eq!(text_result(&home), "分段文字", "文字拆在子元素里也该找到");
    let ambiguous = String::from_utf8(run(&home, &["click", "text=编辑"]).stdout).unwrap();
    assert!(
        ambiguous.contains("2 个元素"),
        "同名的两个按钮都能点时应报错，不替 agent 猜：{ambiguous}"
    );
    let missing = String::from_utf8(run(&home, &["click", "text=不存在的文字"]).stdout).unwrap();
    assert!(missing.contains("没有文字恰好是"), "{missing}");
    // fill 找到的是标签文字时，填进标签关联的输入框；目标不是输入框时报错，不往别的框里打字
    assert_eq!(
        json(&home, &["fill", "text=姓名", "按标签填"])["value"],
        "按标签填"
    );
    let not_input = String::from_utf8(run(&home, &["fill", "text=分段文字", "x"]).stdout).unwrap();
    assert!(not_input.contains("拿不到输入焦点"), "{not_input}");
    assert_eq!(
        json(&home, &["eval", "document.querySelector('#name').value"])["result"],
        "按标签填",
        "目标不是输入框时不该往之前有焦点的输入框里打字"
    );
    // 选择器命中多个时先取可见的：第一个 input[name=dup] 是隐藏的
    assert_eq!(json(&home, &["fill", "input[name=dup]", "x"])["value"], "x");
    assert_eq!(
        json(&home, &["eval", "document.querySelector('#dup2').value"])["result"],
        "x"
    );
    assert_eq!(
        json(
            &home,
            &["wait", "--selector", "input[name=dup]", "--timeout", "2000"]
        )["ok"],
        true,
        "wait 应看所有命中里有没有可见的"
    );
    // fill 失败时写明实际原因
    let redirected = String::from_utf8(run(&home, &["fill", "#redir", "y"]).stdout).unwrap();
    assert!(
        redirected.contains("焦点被页面转到了 <input#real>"),
        "{redirected}"
    );
    let disabled = String::from_utf8(run(&home, &["fill", "#off", "y"]).stdout).unwrap();
    assert!(disabled.contains("已禁用"), "{disabled}");
    let hidden = String::from_utf8(run(&home, &["fill", "input[style]", "y"]).stdout).unwrap();
    assert!(hidden.contains("不可见"), "{hidden}");
    // 填完之后目标的文字变了：值要从当前焦点读回，不能重新定位一次目标（那时已经找不到了）
    assert_eq!(json(&home, &["fill", "text=计数 0", "abc"])["value"], "abc");
    // 空文字加 --append 什么都不删：之前会发一次 Backspace，把最后一个字删掉
    assert_eq!(
        json(&home, &["fill", "#live", "", "--append"])["value"],
        "abc"
    );

    // 密码框：stdout 照旧返回真实的 value，只多一个 masked 标记；日志里的 argv 换成 ***
    let password = json(&home, &["fill", "#pw", "p@ssw0rd-smoke"]);
    assert_eq!(password["value"], "p@ssw0rd-smoke", "{password}");
    assert_eq!(password["masked"], true, "{password}");
    // type 打进密码框同理，看的是当前焦点
    let typed = json(&home, &["type", "typed-secret-9"]);
    assert_eq!(typed["masked"], true, "{typed}");

    let selected = json(&home, &["select", "#country", "Germany"]);
    assert_eq!(selected["value"], "DE");

    let new_tab = json(&home, &["click", "#new-tab"]);
    assert!(
        !new_tab["changes"]["new_tabs"]
            .as_array()
            .unwrap()
            .is_empty(),
        "未检测到新标签页：{new_tab}"
    );

    // 新标签页开出来后原标签页退到后台，鼠标事件不该为此等 5 秒
    let started = std::time::Instant::now();
    json(&home, &["click", "#flash"]);
    assert!(
        started.elapsed().as_millis() < 3_000,
        "后台标签页上的点击变慢了：{:?}",
        started.elapsed()
    );

    // 对话框默认按取消处理
    let dismissed = json(&home, &["click", "#confirm"]);
    assert_eq!(dismissed["dialogs"][0]["type"], "confirm", "{dismissed}");
    assert_eq!(dismissed["dialogs"][0]["handled"], "dismiss");
    assert_eq!(
        json(&home, &["wait", "--text", "已取消", "--timeout", "3000"])["ok"],
        true
    );
    let accepted = json(&home, &["click", "#confirm", "--on-dialog", "accept"]);
    assert_eq!(accepted["dialogs"][0]["handled"], "accept", "{accepted}");
    assert_eq!(
        json(&home, &["wait", "--text", "已确认", "--timeout", "3000"])["ok"],
        true
    );
    // eval 里弹出的对话框同样当场处理，不会卡住
    let alerted = json(&home, &["eval", "alert('提示'); 1"]);
    assert_eq!(alerted["result"], 1, "{alerted}");
    assert_eq!(alerted["dialogs"][0]["type"], "alert");

    json(&home, &["upload", "#file", &fixture_text]);
    assert_eq!(
        json(
            &home,
            &["wait", "--text", "fixture.html", "--timeout", "3000"]
        )["ok"],
        true
    );
    // 隐藏的 file 输入框（自定义上传按钮的常见写法）照样能设文件
    json(&home, &["upload", "#hidden-file", &fixture_text]);
    assert_eq!(
        json(
            &home,
            &[
                "eval",
                "document.querySelector('#hidden-file').files[0]?.name"
            ]
        )["result"],
        "fixture.html"
    );

    let snapshot = ok(&home, &["snapshot"]);
    // 开放 shadow root 里的按钮：遮挡检查在元素自己的根里取命中元素，不会把宿主当成遮挡物
    let shadow_ref = ref_for(&snapshot, "影子按钮");
    json(&home, &["click", &shadow_ref]);
    assert_eq!(
        json(
            &home,
            &["eval", "document.querySelector('#text-result').textContent"]
        )["result"],
        "影子按钮"
    );
    let iframe_ref = ref_for(&snapshot, "iframe按钮");
    json(&home, &["click", &iframe_ref]);
    assert_eq!(
        json(
            &home,
            &["wait", "--text", "iframe 已点击", "--timeout", "3000"]
        )["ok"],
        true
    );
    // text 读整页或只读某个元素
    assert!(ok(&home, &["text"]).contains("保存"));
    assert_eq!(
        ok(&home, &["text", "#iframe-result"]).trim(),
        "iframe 已点击"
    );
    // text= 也找得到同源 iframe 里的元素
    ok(
        &home,
        &[
            "eval",
            "document.querySelector('#iframe-result').textContent = ''",
        ],
    );
    json(&home, &["click", "text=iframe按钮"]);
    assert_eq!(
        json(
            &home,
            &["wait", "--text", "iframe 已点击", "--timeout", "3000"]
        )["ok"],
        true
    );

    let shot = home.join("smoke.png");
    let shot_text = shot.to_string_lossy().into_owned();
    let screenshot = json(&home, &["screenshot", &shot_text, "--annotate"]);
    assert!(shot.is_file(), "截图文件不存在：{screenshot}");
    assert!(
        screenshot["width"].as_u64().unwrap() > 0 && screenshot["height"].as_u64().unwrap() > 0
    );
    // 输出带截图时的页面地址、标题、时间，事后能核对图和商品页对不对得上
    assert!(
        screenshot["url"]
            .as_str()
            .unwrap()
            .ends_with("fixture.html"),
        "{screenshot}"
    );
    assert_eq!(screenshot["title"], "webctl smoke fixture", "{screenshot}");
    let taken_at = screenshot["taken_at"].as_str().unwrap();
    assert!(
        taken_at.len() == 25 && &taken_at[10..11] == "T",
        "taken_at 应是带时区的本地时间：{taken_at}"
    );
    assert_eq!(screenshot["overwrote"], false, "{screenshot}");
    let again = json(&home, &["screenshot", &shot_text]);
    assert_eq!(
        again["overwrote"], true,
        "同一路径再截一次应报覆盖：{again}"
    );
    let jpg = home.join("smoke.jpg");
    let jpg_shot = json(&home, &["screenshot", &jpg.to_string_lossy()]);
    assert_eq!(&fs::read(&jpg).unwrap()[..2], [0xFF, 0xD8], "应保存为 JPEG");
    assert_eq!(
        jpg_shot["width"], screenshot["width"],
        "JPEG 宽度应和 PNG 一致"
    );

    // 视觉识别：从截图里裁出 #flash 按钮当模板，find 应找回它的坐标，
    // clickat/vclick 应能不经过 DOM 选择器点出点击效果
    let plain_shot = home.join("plain.png");
    json(&home, &["screenshot", &plain_shot.to_string_lossy()]);
    let rect = json(
        &home,
        &[
            "eval",
            "(() => { const r = document.querySelector('#flash').getBoundingClientRect(); return {x: r.x, y: r.y, w: r.width, h: r.height, dpr: devicePixelRatio}; })()",
        ],
    )["result"]
        .clone();
    let (rx, ry) = (rect["x"].as_f64().unwrap(), rect["y"].as_f64().unwrap());
    let (rw, rh) = (rect["w"].as_f64().unwrap(), rect["h"].as_f64().unwrap());
    let dpr = rect["dpr"].as_f64().unwrap();
    let template = home.join("flash-template.png");
    crop_png(
        &plain_shot,
        &template,
        (rx * dpr) as usize,
        (ry * dpr) as usize,
        (rw * dpr) as usize,
        (rh * dpr) as usize,
    );
    let template_text = template.to_string_lossy().into_owned();

    let found = json(&home, &["find", &template_text]);
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "find 应找到唯一匹配：{found}");
    let (fx, fy) = (
        matches[0]["x"].as_f64().unwrap(),
        matches[0]["y"].as_f64().unwrap(),
    );
    assert!(
        (fx - (rx + rw / 2.0)).abs() < 3.0 && (fy - (ry + rh / 2.0)).abs() < 3.0,
        "匹配中心应在按钮中心附近：find 报 {fx},{fy}，实际 {},{}",
        rx + rw / 2.0,
        ry + rh / 2.0
    );

    assert_eq!(json(&home, &["move", "40", "40"])["ok"], true);

    let clicked = json(&home, &["clickat", &fx.to_string(), &fy.to_string()]);
    assert!(
        clicked["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "保存成功"),
        "clickat 应点中按钮：{clicked}"
    );

    let vclicked = json(&home, &["vclick", &template_text]);
    assert!(
        vclicked["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "保存成功"),
        "vclick 应点中按钮：{vclicked}"
    );
    assert!(
        vclicked["match"]["score"].as_f64().unwrap() > 0.9,
        "裁出来的模板应接近满分匹配：{vclicked}"
    );

    // 模板不是有效图片时报错退出，不许假装找了
    let miss = run(&home, &["vclick", &fixture_text]);
    assert!(!miss.status.success(), "模板不是 PNG 时应报错");

    // 长按和拖动：fixture 的按钮要按住不少于 1500ms 才算完成，滑块要按住左键拖到最右才算完成
    let layout = json(
        &home,
        &[
            "eval",
            "document.querySelector('#track').scrollIntoView({block: 'center'}); const box = s => { const r = document.querySelector(s).getBoundingClientRect(); return {x: r.x + r.width / 2, y: r.y + r.height / 2, right: r.right}; }; return {hold: box('#hold'), knob: box('#knob'), track: box('#track')};",
        ],
    )["result"]
        .clone();
    let coord = |item: &str, key: &str| layout[item][key].as_f64().unwrap().to_string();
    let input_log = |home: &Path| json(home, &["eval", "inputLog"])["result"].clone();
    json(
        &home,
        &["clickat", &coord("hold", "x"), &coord("hold", "y")],
    );
    assert_eq!(input_log(&home)["hold"], "", "普通点击不该算按住");
    let started = std::time::Instant::now();
    json(
        &home,
        &[
            "clickat",
            &coord("hold", "x"),
            &coord("hold", "y"),
            "--hold",
            "1600",
        ],
    );
    assert!(started.elapsed().as_millis() >= 1_600);
    assert_eq!(input_log(&home)["hold"], "按住完成");
    json(
        &home,
        &[
            "drag",
            &coord("knob", "x"),
            &coord("knob", "y"),
            &coord("track", "right"),
            &coord("knob", "y"),
        ],
    );
    let log = input_log(&home);
    assert_eq!(log["slider"], "拖动完成", "{log}");
    assert!(
        log["dragMoves"].as_u64().unwrap() >= 10,
        "拖动途中的移动事件要带着按住的左键：{log}"
    );
    assert_eq!(
        log["untrusted"], 0,
        "页面收到的鼠标事件都应是 isTrusted：{log}"
    );

    // 拦截页要如实报出来，不能让 agent 把空白验证页当成目标内容
    let challenge = root.join("tests/challenge.html");
    let blocked_page = json(&home, &["open", &challenge.to_string_lossy()]);
    assert_eq!(blocked_page["blocked"], "cloudflare", "{blocked_page}");
    assert!(
        blocked_page["hint"].as_str().unwrap().contains("front"),
        "拦截页提示应告诉 agent 叫人来处理：{blocked_page}"
    );
    // 其他厂商的拦截页、错误页也要报出类型；识别只作提示，命令照常成功
    let blocked_dir = root.join("tests/blocked");
    for (file, kind) in [
        ("px.html", "perimeterx"),
        ("akamai.html", "akamai"),
        ("datadome.html", "datadome"),
        ("puzzle.html", "puzzle"),
        ("splashui/challenge.html", "ebay_interstitial"),
        ("other.html", "other"),
    ] {
        let page = json(&home, &["open", &blocked_dir.join(file).to_string_lossy()]);
        assert_eq!(page["blocked"], kind, "{page}");
        assert!(page["hint"].is_string(), "拦截页应附提示：{page}");
        assert!(
            !page["hint"].as_str().unwrap().contains("交给用户"),
            "怎么处理由调用方决定，提示不写死交给用户：{page}"
        );
    }
    // 点筛选、翻页后才跳出来的拦截页不经过 open：eval、back、reload 的输出也要带；eval 报错时也带
    assert_eq!(json(&home, &["eval", "1"])["blocked"], "other");
    let failed_on_blocked =
        String::from_utf8(run(&home, &["eval", "noSuchData.price"]).stdout).unwrap();
    assert!(
        failed_on_blocked.contains("\"blocked\":\"other\""),
        "{failed_on_blocked}"
    );
    let backed = json(&home, &["back"]);
    assert_eq!(backed["blocked"], "ebay_interstitial", "{backed}");
    let reloaded = json(&home, &["reload"]);
    assert_eq!(reloaded["blocked"], "ebay_interstitial", "{reloaded}");
    // 提示按类型写
    let px = json(
        &home,
        &["open", &blocked_dir.join("px.html").to_string_lossy()],
    );
    assert!(px["hint"].as_str().unwrap().contains("--hold"), "{px}");
    let akamai = json(
        &home,
        &["open", &blocked_dir.join("akamai.html").to_string_lossy()],
    );
    assert!(
        akamai["hint"].as_str().unwrap().contains("拒绝访问"),
        "{akamai}"
    );

    // fixture 里有一个 display:none 的 captcha 容器，文字是"拖动"：没渲染出来的不算拼图验证页
    let normal = json(&home, &["open", &fixture_text]);
    assert!(normal["blocked"].is_null(), "正常页面不该报拦截：{normal}");
    // 标题是 Access Denied，但正文很长、也没有 Reference #（比如某个站自己的权限说明页）：不算拦截
    let titled = json(
        &home,
        &[
            "eval",
            "document.title = 'Access Denied'; document.body.append('正文'.repeat(3000)); 1",
        ],
    );
    assert!(
        titled["blocked"].is_null(),
        "只有标题像不该报拦截：{titled}"
    );
    // 正文很长的页面里嵌了 Turnstile 勾选框（评论、结账表单里常见）：是正常页面，不算拦截
    let embedded = json(
        &home,
        &[
            "eval",
            "document.body.append(Object.assign(document.createElement('div'), {className: 'cf-turnstile'})); 1",
        ],
    );
    assert!(
        embedded["blocked"].is_null(),
        "长正文页面里的勾选框不该报拦截：{embedded}"
    );

    // 控件是页面里的普通 iframe：直接量 iframe 的位置
    let widget_page = root.join("tests/turnstile.html");
    let widget_opened = json(&home, &["open", &widget_page.to_string_lossy()]);
    assert_eq!(widget_opened["blocked"], "cloudflare", "{widget_opened}");
    let clicked = json(&home, &["turnstile", "--timeout", "5000"]);
    assert_eq!(clicked["challenge"], "widget_visible", "{clicked}");
    assert_eq!(clicked["measured_from"], "iframe", "{clicked}");
    assert_eq!(
        clicked["passed"], true,
        "内嵌控件通过后页面不跳转，要靠 token 判定：{clicked}"
    );
    assert_eq!(clicked["title"], "turnstile passed", "{clicked}");

    // 已经通过的控件不该再去点
    let again = json(&home, &["turnstile", "--timeout", "5000"]);
    assert_eq!(again["passed"], true, "{again}");
    assert_eq!(again["clicked"], false, "已通过的控件不该再点：{again}");
    // 控件还在，但 token 已经写入：不再算拦截页
    assert!(json(&home, &["eval", "1"])["blocked"].is_null());

    // 真实 Turnstile 的结构：iframe 在封闭 shadow root 里，JS 够不着，只能量容器
    let shadow_page = root.join("tests/turnstile-shadow.html");
    let shadow_opened = json(&home, &["open", &shadow_page.to_string_lossy()]);
    assert_eq!(shadow_opened["blocked"], "cloudflare", "{shadow_opened}");
    let shadow_clicked = json(&home, &["turnstile", "--timeout", "5000"]);
    assert_eq!(
        shadow_clicked["challenge"], "widget_visible",
        "{shadow_clicked}"
    );
    assert_eq!(
        shadow_clicked["measured_from"], "container",
        "封闭 shadow root 里的 iframe 取不到，应退回用容器的位置：{shadow_clicked}"
    );
    assert_eq!(
        shadow_clicked["passed"], true,
        "按容器坐标点也应落进 shadow 里的 iframe：{shadow_clicked}"
    );

    // 整页验证：标题正常，主文档里只有 0×0 的隐藏字段，勾选框在它父元素的封闭 shadow root 里，4 秒后才出现。
    // 要量隐藏字段的父元素、量的时间要够长，而且控件还在时不能没点就报通过
    let managed = root.join("tests/turnstile-managed.html");
    let managed_opened = json(&home, &["open", &managed.to_string_lossy()]);
    assert_eq!(managed_opened["blocked"], "cloudflare", "{managed_opened}");
    let managed_clicked = json(&home, &["turnstile", "--timeout", "8000"]);
    assert_eq!(
        managed_clicked["clicked"], true,
        "控件还在，不能没点就报通过：{managed_clicked}"
    );
    assert_eq!(
        managed_clicked["measured_from"], "container",
        "{managed_clicked}"
    );
    assert_eq!(
        managed_clicked["passed"], true,
        "按隐藏字段父元素的坐标点，应落进 shadow 里的 iframe：{managed_clicked}"
    );

    // 没有可见控件时不许假装点了（--timeout 3000：量 3 秒就够，不用等满 10 秒）
    json(&home, &["open", &challenge.to_string_lossy()]);
    let untouched = json(&home, &["turnstile", "--timeout", "3000"]);
    assert_eq!(untouched["challenge"], "widget_hidden", "{untouched}");
    assert_eq!(untouched["clicked"], false, "{untouched}");

    // 两个会话共用一个 Chrome：smoke 关掉自己的当前页后，不能接管 smoke2 正在用的页
    let endpoint = json(&home, &["status"])["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = json_as(&home, "smoke2", &["--cdp", &endpoint, "tab", "new"])["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    // 先把 smoke 其他的页关掉，只剩 smoke 的当前页和 smoke2 的页：按标签页顺序选的话下一个就是 smoke2 的
    let tabs = json(&home, &["tabs"]);
    let mine = tabs["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tab| tab["current"] == true)
        .unwrap()["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    for tab in tabs["tabs"].as_array().unwrap() {
        let id = tab["targetId"].as_str().unwrap();
        if id != mine && id != other {
            let closed = json(&home, &["tab", "close", id]);
            assert_eq!(
                closed["current"],
                mine.as_str(),
                "关别的页时当前页不该变：{closed}"
            );
        }
    }
    // 剩下的页是 smoke2 的当前页：不接管，也不新建空白页，当前页留空
    let closed = json(&home, &["tab", "close"]);
    assert_eq!(closed["closed"], mine.as_str(), "{closed}");
    assert!(
        closed["current"].is_null(),
        "关掉自己的页后不该接管别的会话的页，也不该新建空白页：{closed}"
    );
    // 第三个会话用 --cdp 第一次连上，也不接管 smoke2 的当前页；没有可选的页时当前页留空，不为它新建
    let third = json_as(&home, "smoke3", &["--cdp", &endpoint, "tabs"]);
    let third_tabs = third["tabs"].as_array().unwrap();
    assert!(
        third_tabs
            .iter()
            .all(|tab| tab["current"] != true || tab["targetId"] != other.as_str()),
        "{third}"
    );
    // 通道收尾只 tab close、不删状态文件，下一轮换新的会话名：收尾后标签页数不能增加。
    // 关掉的页可能还在列表里残留一会儿，比较时去掉；smoke2 的当前页一直在，用它列标签页不会新建页
    let page_ids = |skip: &[&str]| -> Vec<String> {
        json_as(&home, "smoke2", &["tabs"])["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tab| tab["targetId"].as_str())
            .filter(|id| !skip.contains(id))
            .map(str::to_owned)
            .collect()
    };
    let before = page_ids(&[&mine]);
    json_as(&home, "lane1", &["--cdp", &endpoint, "open", &fixture_text]);
    let lane_closed = json_as(&home, "lane1", &["tab", "close"]);
    assert!(lane_closed["current"].is_null(), "{lane_closed}");
    let lane_tab = lane_closed["closed"].as_str().unwrap();
    assert_eq!(
        page_ids(&[&mine, lane_tab]),
        before,
        "通道收尾后不该多出空白页"
    );
    // 按 concurrency.md 初始化通道：--cdp 连上直接 tab new。连接时不该先建一个空白页，tab new 之后它就没人用了
    let lane2_tab = json_as(&home, "lane2", &["--cdp", &endpoint, "tab", "new"])["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    json_as(&home, "lane2", &["tab", "close"]);
    assert_eq!(
        page_ids(&[&mine, lane_tab, &lane2_tab]),
        before,
        "--cdp tab new 再 tab close 后不该多出空白页"
    );

    // 日志里不能留下明文密码：fill、type 的那段文字要换成 ***
    let db = rusqlite::Connection::open(home.join("webctl.db")).unwrap();
    let fill_argv: String = db
        .query_row(
            "SELECT argv FROM commands WHERE command = 'fill' AND argv LIKE '%#pw%' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(fill_argv.contains("***"), "密码应换成 ***：{fill_argv}");
    let leaked: i64 = db
        .query_row(
            "SELECT count(*) FROM commands WHERE argv LIKE '%p@ssw0rd-smoke%' OR argv LIKE '%typed-secret-9%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "整张表里都不该出现明文密码");

    json(&home, &["close"]);
    let status = json(&home, &["status"]);
    assert_eq!(
        status["reachable"], false,
        "close 后 status 不应重新启动浏览器：{status}"
    );
}

/// 从 PNG 里裁一块矩形存成新 PNG，给 find/vclick 当模板
fn crop_png(src: &Path, dst: &Path, x: usize, y: usize, w: usize, h: usize) {
    let mut decoder = png::Decoder::new(fs::File::open(src).unwrap());
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    let data = &buf[..info.buffer_size()];
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb | png::ColorType::Indexed => 3,
        png::ColorType::Rgba => 4,
    };
    let sw = info.width as usize;
    assert!(
        x + w <= sw && y + h <= info.height as usize,
        "裁剪区域越界：{x},{y} {w}x{h} 超出 {}x{}",
        info.width,
        info.height
    );
    let mut out = vec![0u8; w * h * channels];
    for row in 0..h {
        let s = ((y + row) * sw + x) * channels;
        out[row * w * channels..(row + 1) * w * channels]
            .copy_from_slice(&data[s..s + w * channels]);
    }
    let mut encoder = png::Encoder::new(fs::File::create(dst).unwrap(), w as u32, h as u32);
    encoder.set_color(info.color_type);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&out).unwrap();
}
