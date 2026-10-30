# claude-sessions 改造方案：多主机分栏 + 地址列

日期：2026-10-30　仓库：`F:\BaiduSyncdisk\version20240608\main_code_space\other\claude-sessions`（上游 Nicsilver/claude-sessions，分支 `pj-remote-hosts`，基于 tag `v0.17.9`）

## 1. 目标

1. 面板从上到下分栏：**本机**在最上面，下面每个远程主机一栏（第一个是 zcm6）。远程主机写在配置文件里，不写死 ssh，也不写死 zcm6。
2. 每一行加一列**地址**（会话的 cwd）。地址显示成短形式，点一下展开详情，能看到完整路径。
3. 本机原有的行为全部保留。改动尽量集中在新文件里，方便以后跟上游 rebase。

## 2. 现状（v0.17.9 代码要点）

- 一个二进制同时是 GUI（Tauri 2）、`record` hook 记录器和安装器。
- 记录器往 `~/.claude/session-status/state/<sid>.json` 写状态。GUI 的心跳线程（`gui.rs`）调 `model::load()` 读这个目录，再 `emit("sessions", 列表)` 推给 `ui/index.html` 的 `render()`。窗口宽度固定 328，高度由 `fitHeight()` 自适应。
- `model::Sess` 里没有 cwd。cwd 在状态文件里有，只是没有读进来。
- Linux 支持：`platform.rs` 已经用 `/proc` 实现了进程起始时间；但 `tauri` 是无条件依赖，Linux 上编译要装 webkit，没法直接拿去 zcm6 用。
- 上游默认会对每个新开会话调一次 `claude -p --model haiku` 自动起名（`ai-label`）。远程主机上也会这样。

## 3. 设计

### 3.1 远程数据通路（通用）

**配置**：Windows 端的 `~/.claude/session-status/remotes.json`。

```json
{
  "remotes": [
    {
      "name": "zcm6",
      "cmd": ["C:\\Windows\\System32\\OpenSSH\\ssh.exe", "-o", "BatchMode=yes",
              "-o", "LogLevel=ERROR", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3",
              "zcm6", "~/mysoft/tools/claude-sessions/0.17.9-pj.1/claude-sessions", "stream"]
    }
  ]
}
```

`cmd` 可以是任意命令，只要它的标准输出符合下面的 stream 协议就行。ssh 别名、跳板机，或者 `wsl.exe -d <distro> -- claude-sessions stream` 都用同一套机制，不对 ssh 做特殊处理。

`LogLevel=ERROR` 屏蔽 OpenSSH 的 post-quantum 警告，否则它会占住 stderr 最后一行、盖掉真正的错误提示。

**远程端新子命令 `claude-sessions stream [interval]`**（interval 默认 2 秒）：
- 每个 interval 读一次状态目录。每行都是一份完整的 JSON 快照：`{"v":1,"host":"ln2","now":…,"sessions":[…]}`。
- 内容有变化时立即输出；没有变化时每 15 秒重发一次，当作心跳。
- 写 stdout 失败（EPIPE）时立即退出。这样 ssh 断开后最多 15 秒，登录节点上就不会再留孤儿进程。

**GUI 新模块 `remote.rs`**：
- 每个远程主机开一个线程去 spawn `cmd`。Windows 上带 `CREATE_NO_WINDOW`，不会闪控制台窗口。
- 逐行解析输出，维护三种状态：`connecting`、`online`、`offline`，同时记下 `last_seen` 和 stderr 的最后一行（作为错误提示）。
- 45 秒没收到任何一行就判为 offline，杀掉子进程重连。重连间隔从 5 秒退避到 60 秒。
- 远程会话只放在内存里，**不写进本机的 state 目录**。这样本机的 pid 清理逻辑不会误删远程会话，远程的数据也不会和本机文件混在一起。

**多登录节点**：
- 记录器在状态文件里新增 `host` 字段（hostname）。
- `model::load()` 只对 `host` 等于本机 hostname 的文件检查 pid 是否存活。其他节点的文件保留，在详情里标出是哪个节点。
- 超过 24 小时没更新的文件当作残留丢弃。
- 旧文件没有 `host` 字段，按本机处理，和上游现在的行为一致。

### 3.2 无头构建（给 Linux 远程主机用）

- 在 Cargo 里加 feature：`default = ["gui"]`，把 `tauri` 及其插件、`auto-launch`、`webview2-com` 改成 optional。`gui.rs`、`selfinstall.rs` 和终端聚焦代码用 `#[cfg(feature = "gui")]` 包起来。
- `cargo build --no-default-features` 得到一个只包含 `record`、`install`、`uninstall`、`markers`、`ai-label`、`stream` 的小二进制。
- 目标平台是 `x86_64-unknown-linux-musl`，静态链接，适配 CentOS 7（glibc 2.17）。在 Windows 上用 `rust-lld` 交叉编译：依赖都是纯 Rust，没有 C 库。如果交叉编译出问题，就退回到在 zcm6 上本地编译。

### 3.3 面板

- 推给前端的数据从一个会话列表改成按栏组织：`[{key:"local", name:"本机", status:"online", sessions:[…]}, {key:"zcm6", name:"zcm6", status, last_seen, error, sessions:[…]}]`。
- 栏的顺序：本机在最上面，远程主机按配置文件里的顺序往下排。
  - 每栏标题显示名称、状态点和会话数。
  - 离线的栏只显示标题“离线 · 3m 前”，会话行隐藏。
  - 远程主机没有会话时显示“无会话”，让你知道连接是通的。
- 顶部的状态计数和托盘图标汇总所有栏；远程会话进入 needs 状态也会触发面板自动弹出。
- **地址列**（仅宽模式）：每行的布局是 色条 | 名称 | 地址 | 时间。
  - 只显示最后一级目录，超出宽度用省略号；鼠标悬停时显示完整路径。
  - 点地址会在这一行下面展开详情，再点一次收起。点地址不会触发跳转到终端。
  - 详情里有：完整路径（可以选中、会自动换行）、主机或节点、状态和消息、session id、最后更新时间，以及一个“复制路径”按钮。
- 窗口宽度：紧凑 328 / 宽 420，由 `fitHeight()` 按当前模式设置。展开详情时高度自动调整。

```text
┌──────────────────────────────────────────────┐
│ ● 1  ● 2  ● 0  ● 3                     + ⚙ × │
├ 本机 ● 3 ────────────────────────────────────┤
│ ▌Claude Code 会话监控   …/temp/check_claude 4s│
│ ▌Happy Coder 对比       …/claude-desktop-ph 12s│
│   ├ F:\BaiduSyncdisk\version20240608\main_cod │
│   │ e_space\other\claude-desktop-phone        │
│   │ 本机 · working · 63315264…  [复制路径]    │
│ ▌au-capability-cost     …/机器学习势函数    5m │
├ zcm6 ● 1 ────────────────────────────────────┤
│ ▌MLP 训练脚本           ~/work/mlp-train    8s │
└──────────────────────────────────────────────┘
```

**远程行的操作**：
- 点击跳转：用标签页标题去本机的 Windows Terminal 里找。ssh 会把 Claude Code 设置的标题带回本机终端，所以可以复用 `terminals/wt.rs` 和 `tabmatch.rs`。找不到就不做任何事。
- 重命名、静音：照常可用，记在本机，按 session id 区分。
- 关闭会话、按 Shift 点击用 AI 起名：远程行上禁用。

## 4. 部署

- **Windows**：
  - 用 rustup 把 Rust 装到 `D:\software\rust`（设置 `RUSTUP_HOME` 和 `CARGO_HOME`，加 `--no-modify-path`），编译器用现有的 `D:\software\vs_build_tools`。
  - 现在的上游 exe 挪到 `D:\software\ClaudeSessions\upstream-0.17.9\` 作为备份，新编出来的 exe 放回原路径。这样 hook 和开机自启都不用改。
- **zcm6**：
  - 二进制放到 `~/mysoft/tools/claude-sessions/<ver>/`，再在 `~/.bash_soft_env` 里加一行带注释的 PATH。不建 `current` 软链：`remotes.json` 直接写带版本号的绝对路径，升级时改一处即可。
  - 运行 `claude-sessions install`：把 hook 写进 zcm6 的 `~/.claude/settings.json`，写之前先备份。
  - 运行 `claude-sessions markers`：把回复末尾标记的规则加进 zcm6 的 `CLAUDE.md`，和 Windows 保持一致。
- 版本号定为 `0.17.9-pj.1`。

## 5. 步骤

1. 装 Rust，先把上游代码原样编译跑通，作为基线。
2. 拆 feature，加 `stream` 子命令和 `host` 字段；交叉编译 musl 版本，在 zcm6 上手动跑一次 `stream`。
3. 写 `remote.rs`，改成按栏推数据，托盘图标汇总所有栏。
4. 改界面：分栏、地址列、详情展开。
5. 部署到 zcm6，替换 Windows 上的 exe。
6. 按第 6 节验收。

每一步单独提交 commit，用 `p-git-commit` 写提交信息。

## 6. 验收

- `cargo test` 覆盖以下内容：
  - stream 快照的格式；
  - 按 `host` 区分的残留清理；
  - 远程输出的逐行解析，包括半行、格式错误的 JSON、协议版本不对的情况；
  - 地址缩写，包括 Windows 路径、Linux 路径、home 替换和中文路径。
- 手动测试：
  - 本机和 zcm6 各开一个 default 权限模式的会话并触发权限请求，对应的栏要变红。这两个会话停掉后要正常消失。
  - 杀掉 ssh 进程，zcm6 那一栏要在 45 秒内变成离线；恢复后自动重连。zcm6 上用 `ps` 检查，不能有残留的 `stream` 进程。
  - 长的中文路径点开详情后要完整显示，复制出来的内容要正确。
  - 本机会话的跳转、重命名、静音、关闭都和改造前一样。

## 7. 风险

- 上游更新很快，10 月就发了好几个版本。fork 之后要跟进上游只能手动 rebase，所以改动越集中越好合并。
- 交叉编译可能卡在某个依赖上。备选方案是在 zcm6 上本地编译。
- Windows 上有两个 `ssh.exe`：系统自带的 OpenSSH 和 git-bash 里的。两个都读 `~/.ssh/config`，以实际测试能连上 zcm6 的那个为准，并在 `cmd` 里写全路径。
- 远程主机上，自动起名也会在新会话开始时调一次 `claude -p --model haiku`，会花少量 token。要不要关掉可以以后再定。

## 8. 已定（2026-10-30 用户确认）

1. fork 推到用户 GitHub（AGPL，源码公开）。
2. 离线主机：隐藏会话行，栏标题保留并显示“离线 · N 分钟前”。
3. 两种尺寸：**紧凑**（328 px，与现在一致，不显示地址列）和**宽**（420 px，显示地址列）。⚙ 设置里切换，记在 config。地址列只显示**最后一级目录**，点击展开详情不变。
